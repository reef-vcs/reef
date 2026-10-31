//! `QueryChanges` over one repository's history index: the filters as one
//! `SELECT` with bound parameters, newest activity first, then each page
//! row's activity, matched touches and paths. Symbol entries are narrowed
//! by display name in SQL and decided by R2.15's rule
//! (`reef_symbols::diff::matches_expand`) in Rust, so there is one
//! matching rule.

use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::HashMap;

use reef_symbols::diff::matches_expand;
use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};

use super::store::Outcome;

/// Activity rows listed per change at most.
pub(crate) const MAX_ACTIVITY: usize = 16;
/// Matched touches or paths listed per change at most.
pub(crate) const MAX_MATCHED: usize = 20;
/// Groups returned at most.
pub(crate) const MAX_GROUPS: usize = 1000;

/// A field changes are counted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Dimension {
    Agent,
    Outcome,
    Workspace,
}

/// A validated `QueryChangesRequest`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Query {
    pub symbols: Vec<String>,
    pub paths: Vec<String>,
    /// `SymbolChangeKind` values.
    pub symbol_changes: Vec<i64>,
    pub agents: Vec<String>,
    /// Workspace ids or hex prefixes.
    pub workspaces: Vec<String>,
    /// Change id hex prefixes.
    pub changes: Vec<String>,
    pub outcomes: Vec<Outcome>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub include_empty: bool,
    pub group_by: Vec<Dimension>,
    /// Rows per page.
    pub limit: usize,
    /// The previous page's last `(last_at, id)`.
    pub after: Option<(i64, i64)>,
}

/// One activity row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Activity {
    pub agent: String,
    pub workspace_id: String,
    pub first_at: i64,
    pub last_at: i64,
    pub operations: i64,
    pub last_operation_id: Option<String>,
}

/// One matched symbol touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Touch {
    pub symbol: String,
    pub old_symbol: Option<String>,
    pub name: String,
    pub kind: i64,
    pub change_kind: i64,
    pub path: String,
}

/// One change of an answer.
#[derive(Debug, Clone, Default)]
pub(crate) struct Row {
    pub id: i64,
    pub change_id: String,
    pub commit_id: String,
    pub outcome: i64,
    pub divergent: bool,
    pub empty: Option<bool>,
    pub subject: String,
    pub author_email: String,
    pub first_at: i64,
    pub last_at: i64,
    pub landed_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub touched_commit: Option<String>,
    pub symbols_coverage: Option<i64>,
    pub paths_changed: i64,
    pub paths_truncated: bool,
    pub symbols_changed: i64,
    pub activity: Vec<Activity>,
    pub activity_count: usize,
    pub touches: Vec<Touch>,
    pub touch_count: usize,
    pub matched_paths: Vec<String>,
}

/// One group's count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Group {
    pub agent: Option<String>,
    pub workspace_id: Option<String>,
    pub outcome: Option<i64>,
    pub count: u64,
}

/// What a query found.
#[derive(Debug, Clone, Default)]
pub(crate) struct Answer {
    pub changes: Vec<Row>,
    /// More changes follow the page.
    pub more: bool,
    pub groups: Vec<Group>,
    pub groups_truncated: bool,
}

/// The display name a symbol entry names: its last descriptor's name, or
/// the entry itself.
pub(crate) fn display_name(entry: &str) -> &str {
    let trimmed = entry.trim_end_matches(['.', '#', '/', ':', '!']);
    let trimmed = trimmed.strip_suffix("()").unwrap_or(trimmed);
    let trimmed = match trimmed.rfind('(') {
        // A disambiguated method: `parse(+1)`.
        Some(at) if trimmed.ends_with(')') => &trimmed[..at],
        _ => trimmed,
    };
    match trimmed.rfind([' ', '/', '#', '.', '`']) {
        Some(at) => &trimmed[at + 1..],
        None => trimmed,
    }
}

/// The SQL condition for a path filter on `column`: the path itself, or
/// under it by whole components.
fn path_condition(column: &str, n: usize, params: &mut Vec<Value>, paths: &[String]) -> String {
    let mut parts = Vec::with_capacity(n);
    for path in paths {
        params.push(Value::Text(path.clone()));
        let at = params.len();
        params.push(Value::Text(format!("{path}/")));
        params.push(Value::Text(format!("{path}0")));
        parts.push(format!(
            "({column} = ?{at} OR ({column} >= ?{} AND {column} < ?{}))",
            at + 1,
            at + 2
        ));
    }
    format!("({})", parts.join(" OR "))
}

fn under(path: &str, filters: &[String]) -> bool {
    filters.is_empty()
        || filters.iter().any(|f| {
            path == f
                || (path.len() > f.len()
                    && path.starts_with(f.as_str())
                    && path.as_bytes()[f.len()] == b'/')
        })
}

/// The touches that match `query`'s symbol filters, by change row id.
fn matched_touches(
    conn: &Connection,
    query: &Query,
) -> rusqlite::Result<BTreeMap<i64, Vec<Touch>>> {
    let mut names: Vec<String> = Vec::new();
    for entry in &query.symbols {
        for name in [display_name(entry), entry.as_str()] {
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_owned());
            }
        }
    }
    let marks = (1..=names.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT t.change, s.symbol, os.symbol, n.name, t.kind, t.change_kind, p.path
         FROM names n
         JOIN touches t ON t.name = n.id
         JOIN symbols s ON s.id = t.symbol
         LEFT JOIN symbols os ON os.id = t.old_symbol
         JOIN paths p ON p.id = t.path
         WHERE n.name IN ({marks})
         ORDER BY t.change, t.seq"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(names.iter()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            Touch {
                symbol: row.get(1)?,
                old_symbol: row.get(2)?,
                name: row.get(3)?,
                kind: row.get(4)?,
                change_kind: row.get(5)?,
                path: row.get(6)?,
            },
        ))
    })?;
    let mut matched: BTreeMap<i64, Vec<Touch>> = BTreeMap::new();
    for row in rows {
        let (change, touch) = row?;
        let symbol_matches = query.symbols.iter().any(|entry| {
            matches_expand(
                entry,
                &touch.symbol,
                touch.old_symbol.as_deref(),
                &touch.name,
            )
        });
        let kind_matches =
            query.symbol_changes.is_empty() || query.symbol_changes.contains(&touch.change_kind);
        if symbol_matches && kind_matches && under(&touch.path, &query.paths) {
            matched.entry(change).or_default().push(touch);
        }
    }
    Ok(matched)
}

/// Runs `query` on one repository's index.
#[expect(
    clippy::too_many_lines,
    reason = "one condition per filter, in one statement"
)]
pub(crate) fn run(conn: &Connection, query: &Query) -> rusqlite::Result<Answer> {
    let mut params: Vec<Value> = Vec::new();
    let mut conds: Vec<String> = Vec::new();
    let symbols = if query.symbols.is_empty() {
        None
    } else {
        let matched = matched_touches(conn, query)?;
        if matched.is_empty() {
            return Ok(Answer::default());
        }
        conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS matched (id INTEGER PRIMARY KEY); DELETE FROM temp.matched;")?;
        {
            let mut insert = conn.prepare_cached("INSERT INTO temp.matched (id) VALUES (?1)")?;
            for id in matched.keys() {
                insert.execute([id])?;
            }
        }
        conds.push("c.id IN (SELECT id FROM temp.matched)".to_owned());
        Some(matched)
    };
    if !query.paths.is_empty() && symbols.is_none() {
        let condition = path_condition("p.path", query.paths.len(), &mut params, &query.paths);
        conds.push(format!(
            "EXISTS (SELECT 1 FROM paths p JOIN change_paths cp ON cp.path = p.id
                     WHERE cp.change = c.id AND {condition})"
        ));
    }
    if !query.outcomes.is_empty() {
        let list = query
            .outcomes
            .iter()
            .map(|o| (*o as i64).to_string())
            .collect::<Vec<_>>()
            .join(",");
        conds.push(format!("c.outcome IN ({list})"));
    }
    if !query.changes.is_empty() {
        let mut parts = Vec::new();
        for prefix in &query.changes {
            params.push(Value::Text(prefix.clone()));
            let at = params.len();
            params.push(Value::Text(format!("{prefix}g")));
            parts.push(format!(
                "(c.change_id >= ?{at} AND c.change_id < ?{})",
                at + 1
            ));
        }
        conds.push(format!("({})", parts.join(" OR ")));
    }
    if !query.include_empty {
        conds.push(
            "(c.empty IS NULL OR c.empty = 0 OR c.touched_commit IS NOT c.commit_id)".to_owned(),
        );
    }
    let activity_cond = activity_condition(query, &mut params);
    if let Some(activity) = &activity_cond {
        conds.push(format!(
            "EXISTS (SELECT 1 FROM activity a JOIN agents ag ON ag.id = a.agent
                     WHERE a.change = c.id AND {activity})"
        ));
    }
    if !query.group_by.is_empty() {
        let answer = groups(conn, query, &conds, activity_cond.as_deref(), &params)?;
        conn.execute_batch("DROP TABLE IF EXISTS temp.matched;")?;
        return Ok(answer);
    }
    if let Some((last_at, id)) = query.after {
        params.push(Value::Integer(last_at));
        let at = params.len();
        params.push(Value::Integer(id));
        conds.push(format!(
            "(c.last_at < ?{at} OR (c.last_at = ?{at} AND c.id < ?{}))",
            at + 1
        ));
    }
    params.push(Value::Integer(
        i64::try_from(query.limit + 1).unwrap_or(i64::MAX),
    ));
    let limit_at = params.len();
    let filter = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conds.join(" AND "))
    };
    let sql = format!(
        "SELECT c.id, c.change_id, c.commit_id, c.outcome, c.divergent, c.empty, c.subject,
                c.author_email, c.first_at, c.last_at, c.landed_at, c.ended_at,
                c.touched_commit, c.symbols_coverage, coalesce(c.paths_changed, 0),
                coalesce(c.paths_truncated, 0), coalesce(c.symbols_changed, 0)
         FROM changes c {filter}
         ORDER BY c.last_at DESC, c.id DESC LIMIT ?{limit_at}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
        Ok(Row {
            id: row.get(0)?,
            change_id: row.get(1)?,
            commit_id: row.get(2)?,
            outcome: row.get(3)?,
            divergent: row.get(4)?,
            empty: row.get(5)?,
            subject: row.get(6)?,
            author_email: row.get(7)?,
            first_at: row.get(8)?,
            last_at: row.get(9)?,
            landed_at: row.get(10)?,
            ended_at: row.get(11)?,
            touched_commit: row.get(12)?,
            symbols_coverage: row.get(13)?,
            paths_changed: row.get(14)?,
            paths_truncated: row.get(15)?,
            symbols_changed: row.get(16)?,
            ..Row::default()
        })
    })?;
    let mut changes = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let more = changes.len() > query.limit;
    changes.truncate(query.limit);
    for change in &mut changes {
        fill(conn, change, query, symbols.as_ref())?;
    }
    if symbols.is_some() {
        conn.execute_batch("DROP TABLE IF EXISTS temp.matched;")?;
    }
    Ok(Answer {
        changes,
        more,
        ..Answer::default()
    })
}

/// The condition on one activity row `a` (joined to its agent `ag`) for
/// the agent, workspace and window filters; `None` when none is set.
fn activity_condition(query: &Query, params: &mut Vec<Value>) -> Option<String> {
    let mut parts = Vec::new();
    if !query.agents.is_empty() {
        let mut marks = Vec::new();
        for agent in &query.agents {
            params.push(Value::Text(agent.clone()));
            marks.push(format!("?{}", params.len()));
        }
        parts.push(format!("ag.agent IN ({})", marks.join(",")));
    }
    if !query.workspaces.is_empty() {
        let mut ors = Vec::new();
        for workspace in &query.workspaces {
            params.push(Value::Text(workspace.clone()));
            let at = params.len();
            params.push(Value::Text(format!("{workspace}g")));
            ors.push(format!(
                "(a.workspace_id >= ?{at} AND a.workspace_id < ?{})",
                at + 1
            ));
        }
        parts.push(format!("({})", ors.join(" OR ")));
    }
    if let Some(since) = query.since {
        params.push(Value::Integer(since));
        parts.push(format!("a.last_at >= ?{}", params.len()));
    }
    if let Some(until) = query.until {
        params.push(Value::Integer(until));
        parts.push(format!("a.first_at < ?{}", params.len()));
    }
    (!parts.is_empty()).then(|| parts.join(" AND "))
}

/// Fills a page row's activity, matched touches and matched paths.
fn fill(
    conn: &Connection,
    change: &mut Row,
    query: &Query,
    symbols: Option<&BTreeMap<i64, Vec<Touch>>>,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT ag.agent, a.workspace_id, a.first_at, a.last_at, a.operations, a.last_operation_id
         FROM activity a JOIN agents ag ON ag.id = a.agent
         WHERE a.change = ?1 ORDER BY a.last_at DESC, ag.agent, a.workspace_id",
    )?;
    let activity = stmt
        .query_map([change.id], |row| {
            Ok(Activity {
                agent: row.get(0)?,
                workspace_id: row.get(1)?,
                first_at: row.get(2)?,
                last_at: row.get(3)?,
                operations: row.get(4)?,
                last_operation_id: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    change.activity_count = activity.len();
    change.activity = activity.into_iter().take(MAX_ACTIVITY).collect();
    if let Some(symbols) = symbols {
        let touches = symbols.get(&change.id).cloned().unwrap_or_default();
        change.touch_count = touches.len();
        change.touches = touches.into_iter().take(MAX_MATCHED).collect();
    } else if !query.paths.is_empty() {
        let mut stmt = conn.prepare_cached(
            "SELECT p.path FROM change_paths cp JOIN paths p ON p.id = cp.path
             WHERE cp.change = ?1",
        )?;
        let mut paths = stmt
            .query_map([change.id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        paths.retain(|path| under(path, &query.paths));
        paths.sort();
        change.matched_paths = paths.into_iter().take(MAX_MATCHED).collect();
    }
    Ok(())
}

/// Counts the changes `conds` keep by `query.group_by`.
fn groups(
    conn: &Connection,
    query: &Query,
    conds: &[String],
    activity: Option<&str>,
    params: &[Value],
) -> rusqlite::Result<Answer> {
    let by_activity = query
        .group_by
        .iter()
        .any(|d| matches!(d, Dimension::Agent | Dimension::Workspace));
    let mut keys = Vec::new();
    for dimension in &query.group_by {
        keys.push(match dimension {
            Dimension::Agent => "ag.agent",
            Dimension::Workspace => "a.workspace_id",
            Dimension::Outcome => "c.outcome",
        });
    }
    let mut all = conds.to_vec();
    let from = if by_activity {
        if let Some(activity) = activity {
            all.push(activity.to_owned());
        }
        "changes c JOIN activity a ON a.change = c.id JOIN agents ag ON ag.id = a.agent"
    } else {
        "changes c"
    };
    let filter = if all.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", all.join(" AND "))
    };
    let key_list = keys.join(", ");
    let sql = format!(
        "SELECT {key_list}, count(DISTINCT c.id) AS n FROM {from} {filter}
         GROUP BY {key_list} ORDER BY n DESC, {key_list} LIMIT {}",
        MAX_GROUPS + 1
    );
    let mut stmt = conn.prepare(&sql)?;
    let dims = query.group_by.clone();
    let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
        let mut group = Group {
            agent: None,
            workspace_id: None,
            outcome: None,
            count: 0,
        };
        for (i, dimension) in dims.iter().enumerate() {
            match dimension {
                Dimension::Agent => group.agent = Some(row.get(i)?),
                Dimension::Workspace => group.workspace_id = Some(row.get(i)?),
                Dimension::Outcome => group.outcome = Some(row.get(i)?),
            }
        }
        group.count = u64::try_from(row.get::<_, i64>(dims.len())?).unwrap_or(0);
        Ok(group)
    })?;
    let mut groups = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let truncated = groups.len() > MAX_GROUPS;
    groups.truncate(MAX_GROUPS);
    Ok(Answer {
        groups,
        groups_truncated: truncated,
        ..Answer::default()
    })
}

/// How much history `conn`'s index holds: the earliest activity and the
/// changes whose touches are pending.
pub(crate) fn coverage(conn: &Connection) -> rusqlite::Result<(Option<i64>, u64)> {
    let oldest: Option<i64> =
        conn.query_row("SELECT min(first_at) FROM activity", [], |row| row.get(0))?;
    let pending: i64 = conn.query_row(
        "SELECT count(*) FROM changes WHERE touched_commit IS NOT commit_id",
        [],
        |row| row.get(0),
    )?;
    Ok((oldest, u64::try_from(pending).unwrap_or(0)))
}

/// Grouped counts keyed for tests.
#[cfg(test)]
pub(crate) fn group_map(answer: &Answer) -> HashMap<(Option<String>, Option<i64>), u64> {
    answer
        .groups
        .iter()
        .map(|g| ((g.agent.clone(), g.outcome), g.count))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::super::store::{self, NewPath, NewTouch, Store, Touches};
    use super::*;

    #[test]
    fn display_names() {
        assert_eq!(display_name("Parser#parse()."), "parse");
        assert_eq!(display_name("Parser#parse()"), "parse");
        assert_eq!(display_name("parse"), "parse");
        assert_eq!(display_name("Parser#"), "Parser");
        assert_eq!(
            display_name("scip-python python pkg 1.0 src/parser/Parser#parse()."),
            "parse"
        );
        assert_eq!(display_name("Foo#bar(+1)."), "bar");
    }

    /// A store with three changes: two agents, paths and touches.
    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = Store::open(&dir.path().join("h.sqlite3"), 2).unwrap();
        store
            .write(|tx| {
                for (i, (change, agent, path, symbol)) in [
                    (
                        "aa01",
                        "bot-1",
                        "src/a.py",
                        "py . . src/a.py/Parser#parse().",
                    ),
                    (
                        "aa02",
                        "bot-2",
                        "src/b.py",
                        "py . . src/b.py/Lexer#tokenize().",
                    ),
                    ("bb03", "bot-1", "srcx/c.py", "py . . srcx/c.py/parse()."),
                ]
                .into_iter()
                .enumerate()
                {
                    let at = i64::try_from(i).unwrap() * 1000 + 1000;
                    let id = store::insert_change(tx, change, "c0", at)?;
                    store::add_activity(tx, id, agent, &format!("ws{i}0000"), at, "op")?;
                    let touches = Touches {
                        paths: vec![NewPath {
                            path: path.to_owned(),
                            kind: 2,
                        }],
                        paths_changed: 1,
                        symbols: vec![NewTouch {
                            symbol: symbol.to_owned(),
                            old_symbol: None,
                            name: display_name(symbol).to_owned(),
                            kind: 8,
                            change_kind: 3,
                            path: path.to_owned(),
                        }],
                        symbols_changed: 1,
                        ..Touches::default()
                    };
                    store::set_touches(tx, id, "c0", at, &touches)?;
                }
                Ok(())
            })
            .unwrap();
        (dir, store)
    }

    fn ids(answer: &Answer) -> Vec<&str> {
        answer
            .changes
            .iter()
            .map(|c| c.change_id.as_str())
            .collect()
    }

    #[test]
    fn filters_compose() {
        let (_dir, store) = fixture();
        let q = |query: Query| {
            store
                .read(|conn| {
                    let limit = if query.limit == 0 { 100 } else { query.limit };
                    run(conn, &Query { limit, ..query })
                })
                .unwrap()
        };
        assert_eq!(ids(&q(Query::default())), ["bb03", "aa02", "aa01"]);
        let by_path = q(Query {
            paths: vec!["src".into()],
            ..Query::default()
        });
        assert_eq!(ids(&by_path), ["aa02", "aa01"], "srcx is not under src");
        assert_eq!(by_path.changes[0].matched_paths, ["src/b.py"]);
        let by_name = q(Query {
            symbols: vec!["parse".into()],
            ..Query::default()
        });
        assert_eq!(ids(&by_name), ["bb03", "aa01"]);
        let by_suffix = q(Query {
            symbols: vec!["Parser#parse()".into()],
            ..Query::default()
        });
        assert_eq!(ids(&by_suffix), ["aa01"]);
        let not_boundary = q(Query {
            symbols: vec!["arser#parse()".into()],
            ..Query::default()
        });
        assert!(not_boundary.changes.is_empty());
        let both = q(Query {
            symbols: vec!["parse".into()],
            paths: vec!["srcx".into()],
            ..Query::default()
        });
        assert_eq!(ids(&both), ["bb03"]);
        let agent = q(Query {
            agents: vec!["bot-1".into()],
            ..Query::default()
        });
        assert_eq!(ids(&agent), ["bb03", "aa01"]);
        let window = q(Query {
            agents: vec!["bot-1".into()],
            since: Some(2500),
            ..Query::default()
        });
        assert_eq!(ids(&window), ["bb03"], "bot-1's earlier change is outside");
        let workspace = q(Query {
            workspaces: vec!["ws1".into()],
            ..Query::default()
        });
        assert_eq!(ids(&workspace), ["aa02"]);
        let change = q(Query {
            changes: vec!["aa".into()],
            ..Query::default()
        });
        assert_eq!(ids(&change), ["aa02", "aa01"]);
        let page = q(Query {
            limit: 1,
            after: Some((3000, 3)),
            ..Query::default()
        });
        assert_eq!(ids(&page), ["aa02"]);
        assert!(page.more);
    }

    #[test]
    fn grouping() {
        let (_dir, store) = fixture();
        let answer = store
            .read(|conn| {
                run(
                    conn,
                    &Query {
                        group_by: vec![Dimension::Agent, Dimension::Outcome],
                        limit: 100,
                        ..Query::default()
                    },
                )
            })
            .unwrap();
        let map = group_map(&answer);
        assert_eq!(map[&(Some("bot-1".into()), Some(1))], 2);
        assert_eq!(map[&(Some("bot-2".into()), Some(1))], 1);
        assert!(answer.changes.is_empty());
    }

    #[test]
    fn query_plans_use_indexes() {
        let (_dir, store) = fixture();
        let plan = |sql: &str| -> String {
            store
                .read(|conn| {
                    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let rows = stmt.query_map([], |row| row.get::<_, String>(3))?;
                    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.join("\n"))
                })
                .unwrap()
        };
        let by_name = plan(
            "SELECT t.change FROM names n JOIN touches t ON t.name = n.id WHERE n.name IN ('parse')",
        );
        assert!(by_name.contains("touches_by_name"), "{by_name}");
        let by_path = plan(
            "SELECT cp.change FROM paths p JOIN change_paths cp ON cp.path = p.id
             WHERE p.path = 'src' OR (p.path >= 'src/' AND p.path < 'src0')",
        );
        assert!(by_path.contains("change_paths_by_path"), "{by_path}");
        let by_agent = plan(
            "SELECT a.change FROM agents ag JOIN activity a ON a.agent = ag.id
             WHERE ag.agent = 'bot-1' AND a.last_at >= 0",
        );
        assert!(by_agent.contains("activity_by_agent"), "{by_agent}");
        let newest = plan("SELECT id FROM changes c ORDER BY c.last_at DESC, c.id DESC LIMIT 10");
        assert!(newest.contains("changes_by_last"), "{newest}");
    }
}
