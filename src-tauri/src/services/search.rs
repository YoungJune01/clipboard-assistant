use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::domain::{ContentKind, GroupId, RecordId, RecordNote};
use crate::services::persistence::{HistoryRecordSummary, PersistenceError};

const DEFAULT_SEARCH_LIMIT: usize = 50;
const MAX_SEARCH_LIMIT: usize = 100;
const MAX_QUERY_CHARACTERS: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchCursor {
    pub score: i64,
    pub captured_at: DateTime<Utc>,
    pub id: RecordId,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchQuery {
    pub query: String,
    pub cursor: Option<SearchCursor>,
    pub limit: usize,
    pub content_kind: Option<ContentKind>,
    pub group_id: Option<GroupId>,
    pub ungrouped_only: bool,
    pub favorites_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPage {
    pub items: Vec<HistoryRecordSummary>,
    pub next_cursor: Option<SearchCursor>,
}

pub(crate) fn create_search_schema(connection: &Connection) -> Result<(), PersistenceError> {
    connection.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS clipboard_search USING fts5(
             record_id UNINDEXED,
             body,
             note,
             source,
             group_name,
             file_names,
             ocr_text,
             qr_text,
             tokenize = 'unicode61 remove_diacritics 2'
         );
         CREATE TRIGGER IF NOT EXISTS clipboard_search_record_deleted
         AFTER DELETE ON clipboard_records BEGIN
             DELETE FROM clipboard_search WHERE record_id = old.id;
         END;",
    )?;
    Ok(())
}

pub(crate) fn rebuild_search_index(connection: &Connection) -> Result<(), PersistenceError> {
    connection.execute("DELETE FROM clipboard_search", [])?;
    let mut statement = connection.prepare("SELECT id FROM clipboard_records")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in ids {
        refresh_search_record(connection, &id)?;
    }
    Ok(())
}

pub(crate) fn refresh_search_record(
    connection: &Connection,
    record_id: &str,
) -> Result<(), PersistenceError> {
    connection.execute(
        "DELETE FROM clipboard_search WHERE record_id = ?1",
        [record_id],
    )?;
    connection.execute(
        "INSERT INTO clipboard_search(
             record_id, body, note, source, group_name, file_names, ocr_text, qr_text
         )
         SELECT r.id,
                COALESCE((SELECT group_concat(p.text_value, char(10))
                          FROM clipboard_representations p
                          WHERE p.record_id = r.id AND p.kind = 'unicode_text'), ''),
                COALESCE(r.note, ''),
                trim(COALESCE(r.source_application, '') || ' ' || COALESCE(r.source_path, '')),
                COALESCE((SELECT g.name FROM clipboard_groups g WHERE g.id = r.group_id), ''),
                COALESCE((SELECT group_concat(p.text_value, char(10))
                          FROM clipboard_representations p
                          WHERE p.record_id = r.id AND p.kind = 'file_list'), ''),
                '',
                ''
         FROM clipboard_records r WHERE r.id = ?1",
        [record_id],
    )?;
    Ok(())
}

pub(crate) fn search_connection(
    connection: &Connection,
    query: SearchQuery,
) -> Result<SearchPage, PersistenceError> {
    let normalized = normalize_query(&query.query);
    if normalized.is_empty() {
        return Ok(SearchPage {
            items: Vec::new(),
            next_cursor: None,
        });
    }
    let requested = if query.limit == 0 {
        DEFAULT_SEARCH_LIMIT
    } else {
        query.limit.min(MAX_SEARCH_LIMIT)
    };
    let like = format!("%{}%", escape_like(&normalized));
    let prefix = format!("{}%", escape_like(&normalized));
    let fts = compile_fts_query(&normalized);
    let mut statement = connection.prepare(
        "WITH candidates AS (
             SELECT r.id, r.captured_at, r.source_application, r.note, r.group_id,
                    r.pinned, r.favorite, r.sensitive, r.content_kind,
                    (SELECT substr(p.text_value, 1, 4096)
                     FROM clipboard_representations p
                     WHERE p.record_id = r.id AND p.kind = 'unicode_text'
                     ORDER BY p.position LIMIT 1) AS preview_text,
                    EXISTS(SELECT 1 FROM clipboard_representations p
                           WHERE p.record_id = r.id AND p.kind IN ('png', 'dib_v5')) AS has_image,
                    (CASE WHEN lower(s.note) = lower(?1) THEN 1200 ELSE 0 END +
                     CASE WHEN lower(s.note) LIKE lower(?2) ESCAPE '\\' THEN 600 ELSE 0 END +
                     CASE WHEN lower(COALESCE(g.name, s.group_name)) LIKE lower(?2) ESCAPE '\\' THEN 500 ELSE 0 END +
                     CASE WHEN lower(s.body) LIKE lower(?2) ESCAPE '\\' THEN 400 ELSE 0 END +
                     CASE WHEN lower(s.file_names) LIKE lower(?2) ESCAPE '\\' THEN 350 ELSE 0 END +
                     CASE WHEN lower(s.ocr_text) LIKE lower(?2) ESCAPE '\\' THEN 325 ELSE 0 END +
                     CASE WHEN lower(s.qr_text) LIKE lower(?2) ESCAPE '\\' THEN 325 ELSE 0 END +
                     CASE WHEN lower(s.source) LIKE lower(?2) ESCAPE '\\' THEN 250 ELSE 0 END +
                     CASE WHEN lower(s.body) LIKE lower(?3) ESCAPE '\\' THEN 100 ELSE 0 END) AS score
             FROM clipboard_search s
             JOIN clipboard_records r ON r.id = s.record_id
             LEFT JOIN clipboard_groups g ON g.id = r.group_id
             WHERE (s.record_id IN (
                        SELECT record_id FROM clipboard_search
                        WHERE clipboard_search MATCH ?4
                    ) OR
                    lower(s.body) LIKE lower(?2) ESCAPE '\\' OR
                    lower(s.note) LIKE lower(?2) ESCAPE '\\' OR
                    lower(s.source) LIKE lower(?2) ESCAPE '\\' OR
                    lower(COALESCE(g.name, s.group_name)) LIKE lower(?2) ESCAPE '\\' OR
                    lower(s.file_names) LIKE lower(?2) ESCAPE '\\' OR
                    lower(s.ocr_text) LIKE lower(?2) ESCAPE '\\' OR
                    lower(s.qr_text) LIKE lower(?2) ESCAPE '\\')
               AND (?5 IS NULL OR r.content_kind = ?5)
               AND (?6 IS NULL OR r.group_id = ?6)
               AND (?7 = 0 OR r.group_id IS NULL)
               AND (?8 = 0 OR r.favorite = 1)
         )
         SELECT id, captured_at, source_application, note, group_id, pinned, favorite,
                sensitive, content_kind, preview_text, has_image, score
         FROM candidates
         WHERE (?9 IS NULL OR score < ?9 OR
                (score = ?9 AND (captured_at < ?10 OR
                 (captured_at = ?10 AND id < ?11))))
         ORDER BY score DESC, captured_at DESC, id DESC
         LIMIT ?12",
    )?;
    let kind = query.content_kind.map(content_kind_value);
    let group_id = query.group_id.map(|id| id.as_uuid().to_string());
    let cursor_score = query.cursor.as_ref().map(|cursor| cursor.score);
    let cursor_time = query
        .cursor
        .as_ref()
        .map(|cursor| cursor.captured_at.to_rfc3339());
    let cursor_id = query
        .cursor
        .as_ref()
        .map(|cursor| cursor.id.as_uuid().to_string());
    let rows = statement.query_map(
        params![
            normalized,
            like,
            prefix,
            fts,
            kind,
            group_id,
            query.ungrouped_only,
            query.favorites_only,
            cursor_score,
            cursor_time,
            cursor_id,
            (requested + 1) as i64,
        ],
        |row| {
            Ok(SearchRow {
                id: row.get(0)?,
                captured_at: row.get(1)?,
                source_application: row.get(2)?,
                note: row.get(3)?,
                group_id: row.get(4)?,
                pinned: row.get(5)?,
                favorite: row.get(6)?,
                sensitive: row.get(7)?,
                content_kind: row.get(8)?,
                text: row.get(9)?,
                has_image: row.get(10)?,
                score: row.get(11)?,
            })
        },
    )?;
    let mut results = Vec::with_capacity(requested + 1);
    for row in rows {
        let row = row?;
        let Ok(item) = row.into_item() else { continue };
        results.push(item);
    }
    let has_more = results.len() > requested;
    results.truncate(requested);
    let next_cursor = has_more.then(|| {
        let last = results.last().expect("search lookahead has a result");
        SearchCursor {
            score: last.1,
            captured_at: last.0.captured_at,
            id: last.0.id,
        }
    });
    Ok(SearchPage {
        items: results.into_iter().map(|(item, _)| item).collect(),
        next_cursor,
    })
}

fn normalize_query(value: &str) -> String {
    value
        .trim()
        .chars()
        .take(MAX_QUERY_CHARACTERS)
        .collect::<String>()
}

fn compile_fts_query(value: &str) -> String {
    value
        .split_whitespace()
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn content_kind_value(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Text => "text",
        ContentKind::RichText => "rich_text",
        ContentKind::Image => "image",
        ContentKind::Files => "files",
    }
}

struct SearchRow {
    id: String,
    captured_at: String,
    source_application: Option<String>,
    note: Option<String>,
    group_id: Option<String>,
    pinned: bool,
    favorite: bool,
    sensitive: bool,
    content_kind: String,
    text: Option<String>,
    has_image: bool,
    score: i64,
}

impl SearchRow {
    fn into_item(self) -> Result<(HistoryRecordSummary, i64), PersistenceError> {
        let content_kind = match self.content_kind.as_str() {
            "text" => ContentKind::Text,
            "rich_text" => ContentKind::RichText,
            "image" => ContentKind::Image,
            "files" => ContentKind::Files,
            _ => return Err(PersistenceError::InvalidData),
        };
        Ok((
            HistoryRecordSummary {
                id: RecordId::parse(&self.id).map_err(|_| PersistenceError::InvalidData)?,
                captured_at: DateTime::parse_from_rfc3339(&self.captured_at)
                    .map_err(|_| PersistenceError::InvalidData)?
                    .with_timezone(&Utc),
                source_application: self.source_application,
                text: self.text,
                has_image: self.has_image,
                content_kind,
                note: self
                    .note
                    .map(RecordNote::new)
                    .transpose()
                    .map_err(|_| PersistenceError::InvalidData)?,
                group_id: self
                    .group_id
                    .map(|value| GroupId::parse(&value))
                    .transpose()
                    .map_err(|_| PersistenceError::InvalidData)?,
                pinned: self.pinned,
                favorite: self.favorite,
                sensitive: self.sensitive,
            },
            self.score,
        ))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::domain::{
        CapturedClipboard, ClipboardRecord, ClipboardRepresentation, ContentIdentity,
        SourceIdentity,
    };
    use crate::services::persistence::{RecordPersistence, SqliteRepository};

    fn record(text: &str, source: &str, offset: i64) -> ClipboardRecord {
        ClipboardRecord::from_capture(CapturedClipboard {
            captured_at: Utc::now() + Duration::seconds(offset),
            source: SourceIdentity {
                application_name: Some(source.to_owned()),
                executable_path: Some(format!("C:\\Apps\\{source}.exe")),
            },
            content_identity: ContentIdentity::new(format!("identity-{text}")),
            representations: vec![ClipboardRepresentation::UnicodeText {
                text: text.to_owned(),
            }],
        })
    }

    #[test]
    fn search_matches_body_note_source_group_and_file_names() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let repository = SqliteRepository::open(path.clone()).unwrap();
        let mut body = record("正文内容", "Browser", 1);
        body.note = Some(RecordNote::new("账户密码").unwrap());
        repository.save_record(&body).unwrap();
        let inspection = Connection::open(path).unwrap();
        let indexed: (String, String, String) = inspection
            .query_row(
                "SELECT body, note, source FROM clipboard_search WHERE record_id = ?1",
                [body.id.as_uuid().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(indexed.0, "正文内容");
        assert_eq!(indexed.1, "账户密码");
        assert!(indexed.2.contains("Browser"));
        let direct: i64 = inspection
            .query_row(
                "SELECT count(*) FROM clipboard_search WHERE clipboard_search MATCH ?1",
                [compile_fts_query("正文")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(direct, 1);

        for query in ["正文", "账户密码", "Browser"] {
            let page = repository
                .search_history(SearchQuery {
                    query: query.to_owned(),
                    ..SearchQuery::default()
                })
                .unwrap();
            assert_eq!(page.items.len(), 1, "query={query}");
        }
    }

    #[test]
    fn exact_note_match_ranks_before_body_match_and_searches_beyond_first_page() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let body = record("账户密码列表", "Editor", 2);
        repository.save_record(&body).unwrap();
        let mut note = record("other", "Editor", 1);
        note.note = Some(RecordNote::new("账户密码").unwrap());
        repository.save_record(&note).unwrap();
        for index in 0..60 {
            repository
                .save_record(&record(&format!("noise-{index}"), "Noise", 100 + index))
                .unwrap();
        }

        let page = repository
            .search_history(SearchQuery {
                query: "账户密码".to_owned(),
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(page.items[0].id, note.id);
        assert!(page.items.iter().any(|item| item.id == body.id));
    }

    #[test]
    fn fts_operators_are_treated_as_literal_input() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        repository
            .save_record(&record("alpha OR beta", "Editor", 0))
            .unwrap();

        let result = repository.search_history(SearchQuery {
            query: "alpha OR beta\"".to_owned(),
            ..SearchQuery::default()
        });
        assert!(result.is_ok());
    }

    #[test]
    fn schema_four_migration_rebuilds_the_search_index() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let expected = record("迁移后可搜索", "Legacy", 0);
        {
            let repository = SqliteRepository::open(path.clone()).unwrap();
            repository.save_record(&expected).unwrap();
        }
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "DROP TRIGGER clipboard_search_record_deleted;
                     DROP TABLE clipboard_search;
                     PRAGMA user_version = 4;",
                )
                .unwrap();
        }

        let repository = SqliteRepository::open(path).unwrap();
        let page = repository
            .search_history(SearchQuery {
                query: "迁移后".to_owned(),
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, expected.id);
    }

    #[test]
    fn note_content_and_delete_mutations_keep_the_index_in_sync() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let mut expected = record("旧正文", "Editor", 0);
        repository.save_record(&expected).unwrap();

        let note = RecordNote::new("更新备注").unwrap();
        repository.update_note(expected.id, Some(&note)).unwrap();
        assert_eq!(
            repository
                .search_history(SearchQuery {
                    query: "更新备注".to_owned(),
                    ..SearchQuery::default()
                })
                .unwrap()
                .items[0]
                .id,
            expected.id
        );

        expected.representations = vec![ClipboardRepresentation::UnicodeText {
            text: "新正文".to_owned(),
        }];
        repository.update_record(&expected).unwrap();
        assert!(
            repository
                .search_history(SearchQuery {
                    query: "旧正文".to_owned(),
                    ..SearchQuery::default()
                })
                .unwrap()
                .items
                .is_empty()
        );
        assert_eq!(
            repository
                .search_history(SearchQuery {
                    query: "新正文".to_owned(),
                    ..SearchQuery::default()
                })
                .unwrap()
                .items[0]
                .id,
            expected.id
        );

        repository.delete_record(expected.id).unwrap();
        assert!(
            repository
                .search_history(SearchQuery {
                    query: "新正文".to_owned(),
                    ..SearchQuery::default()
                })
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn search_filters_and_cursor_paging_are_stable() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let repository = SqliteRepository::open(path.clone()).unwrap();
        let first = record("共同词 第一项", "Editor", 3);
        let mut favorite = record("共同词 收藏项", "Editor", 2);
        favorite.favorite = true;
        let image = ClipboardRecord::from_capture(CapturedClipboard {
            captured_at: Utc::now() + Duration::seconds(1),
            source: SourceIdentity::default(),
            content_identity: ContentIdentity::new("search-image"),
            representations: vec![ClipboardRepresentation::Png {
                bytes: vec![1, 2, 3],
            }],
        });
        repository.save_record(&first).unwrap();
        repository.save_record(&favorite).unwrap();
        repository.save_record(&image).unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "UPDATE clipboard_search SET ocr_text = '共同词 图片项' WHERE record_id = ?1",
                params![image.id.as_uuid().to_string()],
            )
            .unwrap();

        let favorites = repository
            .search_history(SearchQuery {
                query: "共同词".to_owned(),
                favorites_only: true,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(favorites.items.len(), 1);
        assert_eq!(favorites.items[0].id, favorite.id);

        let images = repository
            .search_history(SearchQuery {
                query: "共同词".to_owned(),
                content_kind: Some(ContentKind::Image),
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(images.items.len(), 1);
        assert_eq!(images.items[0].id, image.id);

        let page_one = repository
            .search_history(SearchQuery {
                query: "共同词".to_owned(),
                limit: 1,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(page_one.items.len(), 1);
        let page_two = repository
            .search_history(SearchQuery {
                query: "共同词".to_owned(),
                cursor: page_one.next_cursor,
                limit: 1,
                ..SearchQuery::default()
            })
            .unwrap();
        assert_eq!(page_two.items.len(), 1);
        assert_ne!(page_one.items[0].id, page_two.items[0].id);
    }

    #[test]
    fn group_rename_and_delete_refresh_indexed_group_names() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let group_id = GroupId::new();
        repository.save_group(group_id, "旧分组").unwrap();
        let mut expected = record("普通正文", "Editor", 0);
        expected.group_id = Some(group_id);
        repository.save_record(&expected).unwrap();

        assert_eq!(
            repository
                .search_history(SearchQuery {
                    query: "旧分组".to_owned(),
                    group_id: Some(group_id),
                    ..SearchQuery::default()
                })
                .unwrap()
                .items[0]
                .id,
            expected.id
        );

        repository.save_group(group_id, "新分组").unwrap();
        assert!(
            repository
                .search_history(SearchQuery {
                    query: "旧分组".to_owned(),
                    ..SearchQuery::default()
                })
                .unwrap()
                .items
                .is_empty()
        );
        assert_eq!(
            repository
                .search_history(SearchQuery {
                    query: "新分组".to_owned(),
                    ..SearchQuery::default()
                })
                .unwrap()
                .items[0]
                .id,
            expected.id
        );

        repository.delete_group(group_id).unwrap();
        assert!(
            repository
                .search_history(SearchQuery {
                    query: "新分组".to_owned(),
                    ..SearchQuery::default()
                })
                .unwrap()
                .items
                .is_empty()
        );
    }
}
