use crate::*;

pub(crate) mod knowledge_images;
#[path = "skill_resources.rs"]
mod skill_resources;

#[cfg(test)]
#[path = "persistence_tests.rs"]
pub(crate) mod persistence_tests;
use axum::extract::{Multipart, Path, Query, State};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    collections::HashMap,
    path::{Path as FilePath, PathBuf},
};

fn workspace(app: &App) -> PathBuf {
    app.root.join("data/workspaces/admin")
}
async fn safe_path(base: &FilePath, path: &str, write: bool) -> Result<PathBuf, String> {
    if path.len() > 500
        || FilePath::new(path).is_absolute()
        || FilePath::new(path).components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err("路径必须位于工作目录内".into());
    }
    tokio::fs::create_dir_all(base)
        .await
        .map_err(|e| e.to_string())?;
    let root = tokio::fs::canonicalize(base)
        .await
        .map_err(|e| e.to_string())?;
    let target = root.join(path);
    if write {
        let mut parent = target.parent().ok_or("路径无效")?.to_path_buf();
        while !parent.exists() {
            parent = parent.parent().ok_or("路径无效")?.to_path_buf();
        }
        let resolved = tokio::fs::canonicalize(&parent)
            .await
            .map_err(|e| e.to_string())?;
        if !resolved.starts_with(&root) {
            return Err("路径越过工作目录".into());
        }
        if target.exists()
            && !tokio::fs::canonicalize(&target)
                .await
                .map_err(|e| e.to_string())?
                .starts_with(&root)
        {
            return Err("路径越过工作目录".into());
        }
        tokio::fs::create_dir_all(target.parent().unwrap())
            .await
            .map_err(|e| e.to_string())?;
        return Ok(target);
    }
    let resolved = tokio::fs::canonicalize(target)
        .await
        .map_err(|_| "文件不存在".to_string())?;
    if !resolved.starts_with(&root) {
        return Err("路径越过工作目录".into());
    }
    Ok(resolved)
}
fn runtime<'a>(app: &'a App, image: &'a str) -> crate::workspace_runtime::Runtime<'a> {
    crate::workspace_runtime::Runtime {
        container: "ai-phone-tools",
        image,
        directory: workspace(app),
        skills: app.root.join("data/runtime-skills"),
    }
}
async fn ensure_workspace(app: &App) -> Result<(), String> {
    let mut ready = app.workspace_init.lock().await;
    skill_resources::reconcile(&app.db, &app.root).await?;
    let image = workspace_image();
    let runtime = runtime(app, &image);
    tokio::fs::create_dir_all(runtime.directory.join(".home"))
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::create_dir_all(&runtime.skills)
        .await
        .map_err(|e| e.to_string())?;
    runtime.prepare(&mut ready).await
}
async fn download(app: &App, url: &str, limit: usize) -> Result<Vec<u8>, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "网址无效")?;
    let host = parsed.host_str().ok_or("网址无效")?;
    if parsed.scheme() != "https"
        || host == "localhost"
        || host.ends_with(".local")
        || host.parse::<std::net::IpAddr>().is_ok()
    {
        return Err("请使用公开HTTPS网址".into());
    }
    let proxy = &app.settings.harness_proxy;
    let client = if proxy.enabled() {
        &proxy.downloads
    } else {
        &app.remote
    };
    let response = client
        .get(parsed)
        .header("User-Agent", "AI-Phone/0.2")
        .send()
        .await
        .map_err(|_| "下载连接失败")?;
    if !response.status().is_success() {
        return Err(format!("下载失败：HTTP{}", response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err("下载文件超过大小限制".into());
    }
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(part) = stream.next().await {
        let part = part.map_err(|_| "下载中断")?;
        if bytes.len() + part.len() > limit {
            return Err("下载文件超过大小限制".into());
        }
        bytes.extend_from_slice(&part);
    }
    Ok(bytes)
}
fn metadata(content: &str, fallback: &str) -> (String, String) {
    let mut name = fallback.to_string();
    let mut description = String::new();
    if content.starts_with("---") {
        for line in content.lines().skip(1).take_while(|line| *line != "---") {
            if let Some(value) = line.strip_prefix("name:") {
                name = value.trim().trim_matches(['\"', '\'']).to_string();
            }
            if let Some(value) = line.strip_prefix("description:") {
                description = value.trim().trim_matches(['\"', '\'']).to_string();
            }
        }
    }
    if name.trim().is_empty() {
        name = "自定义 Skill".into();
    }
    (
        name.chars().take(100).collect(),
        description.chars().take(600).collect(),
    )
}
fn content_version(content: &str) -> String {
    crate::request_digest::encode(content.as_bytes())
}
fn validate_skill(content: &str, fallback: &str) -> Result<(String, String), String> {
    if content.trim().is_empty() || content.len() > 1_048_576 || content.contains('\0') {
        return Err("Skill正文为空、包含无效字符或超过1MB".into());
    }
    let (name, description) = metadata(content, fallback);
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return Err("SKILL.md需要包含name和description的YAML头部".into());
    }
    let mut lines = content.lines().skip(1);
    if !lines.by_ref().any(|line| line == "---") || !lines.any(|line| !line.trim().is_empty()) {
        return Err("Skill需要完整YAML头部和可执行的正文说明".into());
    }
    if name == "自定义 Skill" || name.trim().is_empty() || description.trim().is_empty() {
        return Err("Skill需要明确的name和description".into());
    }
    Ok((name, description))
}
async fn skill_set_enabled(
    app: &App,
    id: &str,
    enabled: bool,
    remove: bool,
) -> Result<wire::RuntimeSkill, String> {
    let row = sqlx::query("UPDATE runtime_skills SET enabled=$2,deleted_at=CASE WHEN $3 THEN now() ELSE NULL END WHERE id=$1 AND builtin=false RETURNING *")
        .bind(id).bind(enabled && !remove).bind(remove).fetch_optional(&app.db).await.map_err(|e|e.to_string())?;
    row.as_ref()
        .map(skill)
        .ok_or_else(|| "Skill不存在或内置Skill不可修改".into())
}
async fn skill_edit(
    app: &App,
    input: wire::SkillEditRequest,
) -> Result<wire::RuntimeSkill, String> {
    let (name, description) = validate_skill(&input.content, "")?;
    let mut tx = app.db.begin().await.map_err(|e| e.to_string())?;
    let row =
        sqlx::query("SELECT * FROM runtime_skills WHERE id=$1 AND deleted_at IS NULL FOR UPDATE")
            .bind(&input.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Skill不存在")?;
    let previous = skill(&row);
    if previous.builtin {
        return Err("内置Skill不可修改".into());
    }
    if !crate::request_digest::matches(
        &input.expected_content_version,
        &previous.content_version,
        previous.content.as_bytes(),
    ) {
        return Err("版本冲突：Skill已更新，请重新读取后编辑".into());
    }
    if previous.name.to_lowercase() != name.to_lowercase() {
        return Err("编辑时保留Skill的name；新名称请创建新的Skill".into());
    }
    sqlx::query("UPDATE runtime_skills SET content=$2,description=$3 WHERE id=$1")
        .bind(&input.id)
        .bind(&input.content)
        .bind(description)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let row = skill_resources::commit(tx, &app.db, &app.root, &input.id).await?;
    Ok(skill(&row))
}
pub fn workspace_image() -> String {
    std::env::var("AI_PHONE_WORKSPACE_IMAGE")
        .ok()
        .filter(|image| !image.is_empty())
        .unwrap_or_else(|| "ai-phone-workspace:20261103".into())
}

pub async fn initialize(db: &PgPool, root: &std::path::Path) -> Result<(), String> {
    for builtin in crate::builtin_guides::SKILLS {
        let id = builtin.id;
        let content = tokio::fs::read_to_string(root.join(format!("skills/builtin/{id}/SKILL.md")))
            .await
            .map_err(|e| e.to_string())?;
        let (name, description) = metadata(&content, id);
        sqlx::query("INSERT INTO runtime_skills(id,name,description,source,content,builtin) VALUES($1,$2,$3,'builtin',$4,true) ON CONFLICT(id) DO UPDATE SET content=excluded.content,description=excluded.description")
            .bind(id).bind(name).bind(description).bind(content).execute(db).await.map_err(|e|e.to_string())?;
    }
    skill_resources::reconcile(db, root).await
}
async fn skill_import(
    app: &App,
    source: &str,
    content: &str,
    name: &str,
) -> Result<wire::RuntimeSkill, String> {
    let id = Uuid::now_v7().to_string();
    // Downloads stay outside the workspace mount until the database references them.
    let folder = app.root.join("data/skill-staging").join(&id);
    tokio::fs::create_dir_all(&folder)
        .await
        .map_err(|e| e.to_string())?;
    let text = if !content.trim().is_empty() {
        if content.len() > 1_048_576 {
            return Err("Skill正文过大".into());
        }
        tokio::fs::write(folder.join("SKILL.md"), content)
            .await
            .map_err(|e| e.to_string())?;
        content.to_string()
    } else {
        let parsed = reqwest::Url::parse(source).map_err(|_| "请输入Skill网址")?;
        let parts: Vec<_> = parsed.path().trim_matches('/').split('/').collect();
        if ["github.com", "raw.githubusercontent.com"].contains(&parsed.host_str().unwrap_or(""))
            && parts.len() >= 2
        {
            let owner = parts[0];
            let repo = parts[1];
            let (raw, branch, start) =
                if parsed.host_str() == Some("raw.githubusercontent.com") && parts.len() >= 4 {
                    (true, parts[2], parts[3..].join("/"))
                } else if parts.len() >= 4 && ["tree", "blob"].contains(&parts[2]) {
                    (parts[2] == "blob", parts[3], parts[4..].join("/"))
                } else {
                    (false, "HEAD", String::new())
                };
            let base = if raw || start.ends_with("SKILL.md") {
                FilePath::new(&start)
                    .parent()
                    .unwrap_or(FilePath::new(""))
                    .to_string_lossy()
                    .to_string()
            } else {
                start
            };
            let mut queue = vec![base.clone()];
            let (mut count, mut total) = (0, 0usize);
            while let Some(path) = queue.pop() {
                let url = format!(
                    "https://api.github.com/repos/{owner}/{repo}/contents/{path}?ref={branch}"
                );
                let list: Value = serde_json::from_slice(&download(app, &url, 2_000_000).await?)
                    .map_err(|_| "无法读取GitHub目录")?;
                let entries = list.as_array().ok_or("此地址不是Skill目录")?;
                for entry in entries {
                    let kind = entry["type"].as_str().unwrap_or("");
                    let path = entry["path"].as_str().ok_or("目录无效")?;
                    if entry["name"].as_str().unwrap_or("").starts_with('.') {
                        continue;
                    }
                    if kind == "dir" {
                        if queue.len() > 50 {
                            return Err("Skill子目录过多".into());
                        }
                        queue.push(path.into());
                    } else if kind == "file" {
                        count += 1;
                        if count > 256 {
                            return Err("Skill文件过多，请选择具体Skill目录".into());
                        }
                        let relative = path
                            .strip_prefix(&base)
                            .unwrap_or(path)
                            .trim_start_matches('/');
                        let destination = safe_path(&folder, relative, true).await?;
                        let url = entry["download_url"].as_str().ok_or("无法下载Skill资源")?;
                        let bytes = download(app, url, 5_000_000).await?;
                        total += bytes.len();
                        if total > 25_000_000 {
                            return Err("Skill资源超过25MB".into());
                        }
                        tokio::fs::write(destination, bytes)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            tokio::fs::read_to_string(folder.join("SKILL.md"))
                .await
                .map_err(|_| "此目录没有SKILL.md，请提供具体Skill目录")?
        } else {
            let bytes = download(app, source, 1_048_576).await?;
            let text = String::from_utf8(bytes).map_err(|_| "Skill必须是Markdown文本")?;
            tokio::fs::write(folder.join("SKILL.md"), &text)
                .await
                .map_err(|e| e.to_string())?;
            text
        }
    };
    let (name, description) = validate_skill(&text, name)?;
    skill_resources::seal_staging(&folder).await?;
    let source = if source.is_empty() { "upload" } else { source };
    let mut tx = app.db.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)")
        .bind(name.to_lowercase())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let existing = sqlx::query("SELECT * FROM runtime_skills WHERE lower(name)=lower($1) AND deleted_at IS NULL ORDER BY builtin DESC,created_at LIMIT 1 FOR UPDATE")
        .bind(&name).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
    if let Some(row) = existing {
        let previous = skill(&row);
        let _ = tokio::fs::remove_dir_all(&folder).await;
        if previous.content == text {
            if previous.builtin {
                return Ok(previous);
            }
            sqlx::query("UPDATE runtime_skills SET enabled=true WHERE id=$1")
                .bind(&previous.id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            let row = skill_resources::commit(tx, &app.db, &app.root, &previous.id).await?;
            return Ok(skill(&row));
        }
        return Err(format!(
            "同名Skill已存在（id={}），请读取后使用skill_edit更新，不要重复创建",
            previous.id
        ));
    }
    sqlx::query(
        "INSERT INTO runtime_skills(id,name,description,source,content) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&description)
    .bind(source)
    .bind(&text)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    let row = skill_resources::commit(tx, &app.db, &app.root, &id).await?;
    Ok(skill(&row))
}
fn skill(row: &sqlx::postgres::PgRow) -> wire::RuntimeSkill {
    wire::RuntimeSkill {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        source: row.get("source"),
        content: row.get("content"),
        enabled: row.get("enabled"),
        builtin: row.get("builtin"),
        content_version: content_version(&row.get::<String, _>("content")),
    }
}
fn document(row: &sqlx::postgres::PgRow) -> wire::KnowledgeDocument {
    wire::KnowledgeDocument {
        id: row.get::<Uuid, _>("id").to_string(),
        title: row.get("title"),
        content: row.get("content"),
        revision: row.get::<i64, _>("revision") as u64,
        related_ids: row
            .get::<Vec<Uuid>, _>("related_ids")
            .iter()
            .map(ToString::to_string)
            .collect(),
        trashed: row.try_get::<bool, _>("trashed").unwrap_or(false),
    }
}
async fn skills(app: &App) -> Result<wire::SkillList, String> {
    let rows = sqlx::query(
        "SELECT * FROM runtime_skills WHERE deleted_at IS NULL ORDER BY builtin DESC,created_at",
    )
    .fetch_all(&app.db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(wire::SkillList {
        skills: rows.iter().map(skill).collect(),
    })
}
async fn documents(app: &App, query: &str) -> Result<wire::KnowledgeList, String> {
    let pattern = format!("%{}%", query.chars().take(200).collect::<String>());
    let rows=sqlx::query("SELECT id,title,left(content,300) content,revision,related_ids,(deleted_at IS NOT NULL) trashed FROM knowledge_documents WHERE deleted_at IS NULL AND ($1='%%' OR title ILIKE $1 OR content ILIKE $1) ORDER BY updated_at DESC LIMIT 200").bind(pattern).fetch_all(&app.db).await.map_err(|e|e.to_string())?;
    Ok(wire::KnowledgeList {
        documents: rows.iter().map(document).collect(),
    })
}
async fn knowledge_read(app: &App, id: &str) -> Result<wire::KnowledgeDocument, String> {
    let id = Uuid::parse_str(id).map_err(|_| "文档ID无效")?;
    let row =
        sqlx::query("SELECT *,deleted_at IS NOT NULL trashed FROM knowledge_documents WHERE id=$1")
            .bind(id)
            .fetch_optional(&app.db)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("文档不存在")?;
    Ok(document(&row))
}
async fn knowledge_related(app: &App, id: &str) -> Result<wire::KnowledgeList, String> {
    let doc = knowledge_read(app, id).await?;
    let ids: Vec<Uuid> = doc
        .related_ids
        .iter()
        .filter_map(|s| Uuid::parse_str(s).ok())
        .collect();
    let rows=sqlx::query("SELECT id,title,left(content,300) content,revision,related_ids,false trashed FROM knowledge_documents WHERE id=ANY($1) AND deleted_at IS NULL ORDER BY title,id").bind(ids).fetch_all(&app.db).await.map_err(|e|e.to_string())?;
    Ok(wire::KnowledgeList {
        documents: rows.iter().map(document).collect(),
    })
}
async fn knowledge_link(
    app: &App,
    r: wire::KnowledgeLinkRequest,
) -> Result<wire::KnowledgeDocument, String> {
    let id = Uuid::parse_str(&r.id).map_err(|_| "文档ID无效")?;
    let target = Uuid::parse_str(&r.target_id).map_err(|_| "关联文档ID无效")?;
    if id == target {
        return Err("不能关联文档自身".into());
    }
    let mut tx = app.db.begin().await.map_err(|e| e.to_string())?;
    lock_knowledge_relationships(&mut tx).await?;
    let rows=sqlx::query("SELECT id,revision,related_ids FROM knowledge_documents WHERE id=ANY($1) AND deleted_at IS NULL ORDER BY id FOR UPDATE").bind(vec![id,target]).fetch_all(&mut *tx).await.map_err(|e|e.to_string())?;
    if rows.len() != 2 {
        return Err("关联文档不存在或已移除".into());
    }
    for row in &rows {
        let own: Uuid = row.get("id");
        if own == id && row.get::<i64, _>("revision") != r.expected_revision as i64 {
            return Err("文档已更新，请重新载入后修改关联".into());
        }
        let other = if own == id { target } else { id };
        let mut related: Vec<Uuid> = row.get("related_ids");
        if related.contains(&other) == r.present {
            continue;
        }
        related.retain(|v| *v != other);
        if r.present {
            related.push(other);
        }
        related.sort();
        related.dedup();
        if related.len() > 50 {
            return Err("每篇文档最多关联50篇".into());
        }
        sqlx::query("UPDATE knowledge_documents SET related_ids=$2,revision=revision+1,updated_at=now() WHERE id=$1").bind(own).bind(related).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    knowledge_read(app, &r.id).await
}

async fn lock_knowledge_relationships(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), String> {
    // Both relationship writers share this short transaction lock. No network/file I/O.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('ai-phone:knowledge-relationships'))")
        .execute(&mut **tx)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn synchronize_knowledge_relationships(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    previous: &[Uuid],
    wanted: &[Uuid],
) -> Result<(), String> {
    let neighbors: Vec<Uuid> = previous.iter().chain(wanted).copied().collect();
    let rows = sqlx::query("SELECT id,related_ids,deleted_at IS NOT NULL trashed FROM knowledge_documents WHERE id=ANY($1) ORDER BY id FOR UPDATE")
        .bind(neighbors).fetch_all(&mut **tx).await.map_err(|e| e.to_string())?;
    for target in wanted {
        let row = rows
            .iter()
            .find(|row| row.get::<Uuid, _>("id") == *target)
            .ok_or("关联文档不存在")?;
        // Existing links survive trash/restore, but adding a trashed target is invalid.
        if row.get::<bool, _>("trashed") && !previous.contains(target) {
            return Err("关联文档已移除".into());
        }
    }
    for row in rows {
        let target: Uuid = row.get("id");
        let old: Vec<Uuid> = row.get("related_ids");
        let mut related = old.clone();
        related.retain(|other| *other != id);
        if wanted.contains(&target) {
            related.push(id);
        }
        related.sort();
        related.dedup();
        if related.len() > 50 {
            return Err("每篇文档最多关联50篇".into());
        }
        if related != old {
            sqlx::query("UPDATE knowledge_documents SET related_ids=$2,revision=revision+1,updated_at=now() WHERE id=$1")
                .bind(target).bind(related).execute(&mut **tx).await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub(crate) async fn publish_learned_knowledge(
    app: &App,
    run: Uuid,
    audit: Uuid,
    title: &str,
    summary: &str,
    observations: &[String],
    illustration: Option<&str>,
) -> Result<(), String> {
    let title = app.settings.redact(title);
    let key = title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let summary = app.settings.redact(summary);
    let image = match illustration {
        Some(id) => Some(knowledge_images::observation(app, run, id).await?),
        None => None,
    };
    let mut content = format!("{summary}\n\n来源：任务 `{run}`，核验 `{audit}`。");
    if let Some(image) = &image {
        content.push_str(&image.markdown(&title));
    }
    let mut tx = app.db.begin().await.map_err(|error| error.to_string())?;
    let saved = sqlx::query(
        "INSERT INTO knowledge_documents(id,title,content,learned_key)
         SELECT $1,$2,$3,$4 WHERE EXISTS (
             SELECT 1 FROM app_learning_observations
             WHERE run_id=$5 AND observation_id=ANY($6) AND after_action
         ) ON CONFLICT (learned_key) DO UPDATE
           SET content=knowledge_documents.content || E'\n\n' || EXCLUDED.content,
               revision=knowledge_documents.revision+1,updated_at=now()
           WHERE knowledge_documents.deleted_at IS NULL
             AND strpos(knowledge_documents.content,$7)=0
             AND octet_length(knowledge_documents.content)+octet_length(EXCLUDED.content)+2<=1000000",
    )
    .bind(Uuid::now_v7())
    .bind(title)
    .bind(content)
    .bind(key)
    .bind(run)
    .bind(observations)
    .bind(summary)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    if saved.rows_affected() == 1
        && let Some(image) = image
    {
        image.insert(&mut *tx).await?;
    }
    tx.commit().await.map_err(|error| error.to_string())?;
    Ok(())
}

async fn knowledge_save(
    app: &App,
    input: wire::KnowledgeSaveRequest,
) -> Result<wire::KnowledgeDocument, String> {
    save_document(app, input, false, None).await
}

async fn save_document(
    app: &App,
    mut input: wire::KnowledgeSaveRequest,
    keep_links: bool,
    image: Option<(knowledge_images::Image, String)>,
) -> Result<wire::KnowledgeDocument, String> {
    if let Some((image, alt)) = &image {
        input.content.push_str(&image.markdown(alt));
    }
    if input.title.trim().is_empty()
        || input.title.len() > 400
        || input.content.len() > 1_000_000
        || input.related_ids.len() > 50
    {
        return Err("文档标题为空或内容过大".into());
    }
    let mut related: Vec<Uuid> = input
        .related_ids
        .iter()
        .map(|id| Uuid::parse_str(id))
        .collect::<Result<_, _>>()
        .map_err(|_| "关联文档ID无效")?;
    let id = if input.id.is_empty() {
        Uuid::now_v7()
    } else {
        Uuid::parse_str(&input.id).map_err(|_| "文档ID无效")?
    };
    if related.contains(&id) {
        return Err("不能关联文档自身".into());
    }
    related.sort();
    related.dedup();
    let mut tx = app.db.begin().await.map_err(|e| e.to_string())?;
    lock_knowledge_relationships(&mut tx).await?;
    let previous = if input.id.is_empty() {
        vec![]
    } else {
        let row = sqlx::query("SELECT revision,related_ids FROM knowledge_documents WHERE id=$1 AND deleted_at IS NULL FOR UPDATE")
            .bind(id).fetch_optional(&mut *tx).await.map_err(|e| e.to_string())?;
        let row = row
            .filter(|row| row.get::<i64, _>("revision") == input.expected_revision as i64)
            .ok_or("版本冲突：文档已被更新，请保留草稿并重新载入")?;
        row.get::<Vec<Uuid>, _>("related_ids")
    };
    if keep_links {
        related = previous;
    } else if previous != related {
        synchronize_knowledge_relationships(&mut tx, id, &previous, &related).await?;
    }
    if let Some((image, _)) = image {
        image.insert(&mut *tx).await?;
    }
    if input.id.is_empty() {
        sqlx::query(
            "INSERT INTO knowledge_documents(id,title,content,related_ids) VALUES($1,$2,$3,$4)",
        )
        .bind(id)
        .bind(input.title.trim())
        .bind(&input.content)
        .bind(&related)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    } else {
        sqlx::query("UPDATE knowledge_documents SET title=$2,content=$3,related_ids=$4,revision=revision+1,updated_at=now() WHERE id=$1").bind(id).bind(input.title.trim()).bind(&input.content).bind(&related).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    knowledge_read(app, &id.to_string()).await
}
async fn files(app: &App, path: &str) -> Result<wire::WorkspaceFiles, String> {
    let base = workspace(app);
    tokio::fs::create_dir_all(&base)
        .await
        .map_err(|e| e.to_string())?;
    let folder = safe_path(&base, if path.is_empty() { "." } else { path }, false).await?;
    let mut dir = tokio::fs::read_dir(folder)
        .await
        .map_err(|_| "此路径不是目录")?;
    let mut files = Vec::new();
    while let Some(item) = dir.next_entry().await.map_err(|e| e.to_string())? {
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let metadata = item.metadata().await.map_err(|e| e.to_string())?;
        let relative = FilePath::new(path)
            .join(&name)
            .to_string_lossy()
            .to_string();
        files.push(wire::WorkspaceEntry {
            path: relative,
            name,
            directory: metadata.is_dir(),
            size_bytes: metadata.len(),
        });
        if files.len() >= 500 {
            break;
        }
    }
    files.sort_by(|a, b| b.directory.cmp(&a.directory).then(a.name.cmp(&b.name)));
    Ok(wire::WorkspaceFiles {
        files,
        path: path.into(),
    })
}
fn issue(message: String) -> Response {
    let status = if message.starts_with("版本冲突") {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    error(
        status,
        "workbench_error",
        &message.chars().take(500).collect::<String>(),
    )
}
pub async fn route(
    State(app): State<Shared>,
    Path(operation): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    macro_rules! request {
        ($ty:ty) => {
            match decode::<$ty>(&app, &headers, body) {
                Ok(v) => v,
                Err(e) => return e.into(),
            }
        };
    }
    match operation.as_str() {
        "memories" => {
            let input = request!(wire::WorkbenchRequest);
            let agent = match crate::registry::memory_agent(&app, &input.agent_id).await {
                Ok(v) => v,
                Err(e) => return issue(e),
            };
            match sqlx::query("SELECT id,content,revision FROM memories WHERE owner_id='local-user' AND agent_id=$1 ORDER BY created_at,id").bind(agent).fetch_all(&app.db).await {
                Ok(rows) => pb(StatusCode::OK, wire::MemoryList { memories: rows.iter().map(|r|wire::ModelMemory{id:r.get::<Uuid,_>("id").to_string(),content:r.get("content"),revision:r.get::<i64,_>("revision") as u64}).collect() }), Err(e)=>issue(e.to_string())
            }
        }
        "memory-save" => {
            let input = request!(wire::MemorySaveRequest);
            let agent = match crate::registry::memory_agent(&app, &input.agent_id).await {
                Ok(v) => v,
                Err(e) => return issue(e),
            };
            if input.content.trim().is_empty() || input.content.len() > 4000 {
                return issue("记忆为空或超过4000字节".into());
            }
            let id = if input.id.is_empty() {
                Uuid::now_v7()
            } else {
                match Uuid::parse_str(&input.id) {
                    Ok(id) => id,
                    Err(_) => return issue("记忆ID无效".into()),
                }
            };
            let result = if input.id.is_empty() {
                sqlx::query(
                    "INSERT INTO memories(id,content,agent_id) VALUES($1,$2,$3) RETURNING revision",
                )
                .bind(id)
                .bind(input.content.trim())
                .bind(agent)
                .fetch_optional(&app.db)
                .await
            } else {
                sqlx::query("UPDATE memories SET content=$2,revision=revision+1 WHERE id=$1 AND owner_id='local-user' AND revision=$3 AND agent_id=$4 RETURNING revision").bind(id).bind(input.content.trim()).bind(input.expected_revision as i64).bind(agent).fetch_optional(&app.db).await
            };
            match result {
                Ok(Some(r)) => pb(
                    StatusCode::OK,
                    wire::ModelMemory {
                        id: id.to_string(),
                        content: input.content.trim().into(),
                        revision: r.get::<i64, _>("revision") as u64,
                    },
                ),
                Ok(None) => issue("版本冲突：记忆已更新，请保留草稿并重新载入".into()),
                Err(e) => issue(e.to_string()),
            }
        }
        "memory-delete" => {
            let input = request!(wire::MemoryDeleteRequest);
            let agent = match crate::registry::memory_agent(&app, &input.agent_id).await {
                Ok(v) => v,
                Err(e) => return issue(e),
            };
            let Ok(id) = Uuid::parse_str(&input.id) else {
                return issue("记忆ID无效".into());
            };
            match sqlx::query(
                "DELETE FROM memories WHERE id=$1 AND owner_id='local-user' AND revision=$2 AND agent_id=$3",
            )
            .bind(id)
            .bind(input.expected_revision as i64)
            .bind(agent)
            .execute(&app.db)
            .await
            {
                Ok(v) if v.rows_affected() == 1 => {
                    pb(StatusCode::OK, wire::MemoryList { memories: vec![] })
                }
                Ok(_) => issue("版本冲突：请重新载入记忆".into()),
                Err(e) => issue(e.to_string()),
            }
        }
        "skills" => {
            let _: wire::WorkbenchRequest = request!(wire::WorkbenchRequest);
            match skills(&app).await {
                Ok(v) => pb(StatusCode::OK, v),
                Err(e) => issue(e),
            }
        }
        "skill-import" => {
            let input = request!(wire::SkillImportRequest);
            match skill_import(&app, &input.source, &input.content, &input.name).await {
                Ok(v) => pb(StatusCode::OK, v),
                Err(e) => issue(e),
            }
        }
        "skill-update" => {
            let input = request!(wire::SkillUpdateRequest);
            if let Err(e) = skill_set_enabled(&app, &input.id, input.enabled, input.remove).await {
                return issue(e);
            }
            match skills(&app).await {
                Ok(v) => pb(StatusCode::OK, v),
                Err(e) => issue(e),
            }
        }
        "skill-edit" => {
            let input = request!(wire::SkillEditRequest);
            match skill_edit(&app, input).await {
                Ok(item) => pb(StatusCode::OK, item),
                Err(e) => issue(e),
            }
        }
        "knowledge" => {
            let input = request!(wire::WorkbenchRequest);
            match documents(&app, &input.query).await {
                Ok(v) => pb(StatusCode::OK, v),
                Err(e) => issue(e),
            }
        }
        "knowledge-read" => {
            let input = request!(wire::WorkbenchRequest);
            match knowledge_read(&app, &input.id).await {
                Ok(v) => pb(StatusCode::OK, v),
                Err(e) => issue(e),
            }
        }
        "knowledge-related" => {
            let r = request!(wire::WorkbenchRequest);
            match knowledge_related(&app, &r.id).await {
                Ok(v) => pb(StatusCode::OK, v),
                Err(e) => issue(e),
            }
        }
        "knowledge-link" => {
            let r = request!(wire::KnowledgeLinkRequest);
            match knowledge_link(&app, r).await {
                Ok(v) => pb(StatusCode::OK, v),
                Err(e) => issue(e),
            }
        }
        "knowledge-save" => {
            let input = request!(wire::KnowledgeSaveRequest);
            match knowledge_save(&app, input).await {
                Ok(v) => pb(StatusCode::OK, v),
                Err(e) => issue(e),
            }
        }
        "knowledge-delete" => {
            let input = request!(wire::KnowledgeDeleteRequest);
            let Ok(id) = Uuid::parse_str(&input.id) else {
                return issue("文档ID无效".into());
            };
            match sqlx::query("UPDATE knowledge_documents SET deleted_at=now(),revision=revision+1,updated_at=now() WHERE id=$1 AND revision=$2 AND deleted_at IS NULL").bind(id).bind(input.expected_revision as i64).execute(&app.db).await{Ok(v) if v.rows_affected()==1=>pb(StatusCode::OK,wire::KnowledgeList{documents:vec![]}),Ok(_)=>issue("版本冲突：请重新载入文档".into()),Err(e)=>issue(e.to_string())}
        }
        "files" => {
            let input = request!(wire::WorkbenchRequest);
            match files(&app, &input.id).await {
                Ok(v) => pb(StatusCode::OK, v),
                Err(e) => issue(e),
            }
        }
        "events" => {
            let input = request!(wire::HarnessEventRequest);
            let Ok(id) = Uuid::parse_str(&input.run_id) else {
                return issue("任务ID无效".into());
            };
            let message = if input.message_id.is_empty() {
                None
            } else {
                match Uuid::parse_str(&input.message_id) {
                    Ok(value) => Some(value),
                    Err(_) => return issue("消息ID无效".into()),
                }
            };
            let Ok(after) = i64::try_from(input.after_ms) else {
                return issue("时间范围无效".into());
            };
            let rows=match sqlx::query("SELECT e.id,e.message_id,e.kind,e.title,e.content,e.status,CASE WHEN e.kind='model' THEN e.result ELSE NULL END model_timing,((extract(epoch FROM e.created_at)*1000)::bigint) at_ms,COALESCE((extract(epoch FROM e.finished_at)*1000)::bigint,0) end_ms FROM harness_events e JOIN runs r ON r.id=e.run_id JOIN conversations c ON c.id=r.conversation_id WHERE e.run_id=$1 AND c.owner_id='local-user' AND (e.kind='task' OR (($2::uuid IS NULL OR e.message_id=$2) AND (extract(epoch FROM GREATEST(e.created_at,COALESCE(e.finished_at,e.created_at)))*1000)>$3)) ORDER BY e.created_at,e.id").bind(id).bind(message).bind(after).fetch_all(&app.db).await{Ok(v)=>v,Err(e)=>return issue(e.to_string())};
            pb(
                StatusCode::OK,
                wire::HarnessEventList {
                    events: rows
                        .iter()
                        .map(|r| wire::HarnessEvent {
                            id: r.get::<Uuid, _>("id").to_string(),
                            run_id: id.to_string(),
                            message_id: r.get::<Uuid, _>("message_id").to_string(),
                            kind: crate::run_notice::event_kind(
                                r.get("kind"),
                                r.get("title"),
                                r.get("status"),
                            ),
                            title: r.get("title"),
                            content: r.get("content"),
                            status: r.get("status"),
                            created_at_ms: r.get::<i64, _>("at_ms") as u64,
                            finished_at_ms: r.get::<i64, _>("end_ms") as u64,
                            model_timing: r
                                .get::<Option<Value>, _>("model_timing")
                                .as_ref()
                                .and_then(crate::model_timing::wire),
                        })
                        .collect(),
                },
            )
        }
        _ => error(StatusCode::NOT_FOUND, "not_found", "操作不存在"),
    }
}
pub async fn file(
    State(app): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = auth(&app, &headers) {
        return e.into();
    }
    let path = query.get("path").map(String::as_str).unwrap_or("");
    let file = match safe_path(&workspace(&app), path, false).await {
        Ok(v) => v,
        Err(e) => return issue(e),
    };
    let Ok(metadata) = tokio::fs::metadata(&file).await else {
        return issue("文件不存在".into());
    };
    if !metadata.is_file() {
        return issue("请选择文件".into());
    }
    let stream = match tokio::fs::File::open(&file).await {
        Ok(v) => v,
        Err(e) => return issue(e.to_string()),
    };
    let name = file.file_name().unwrap().to_string_lossy();
    let escaped: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"._-".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    (
        StatusCode::OK,
        [
            ("content-type", "application/octet-stream"),
            ("cache-control", "private,no-store"),
            ("x-content-type-options", "nosniff"),
            (
                "content-disposition",
                &format!("attachment; filename*=UTF-8''{escaped}"),
            ),
        ],
        axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(stream)),
    )
        .into_response()
}
pub(crate) async fn upload_knowledge_image(
    State(app): State<Shared>,
    headers: HeaderMap,
    body: std::result::Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let input = match decode::<wire::KnowledgeImageUploadRequest>(&app, &headers, body) {
        Ok(input) => input,
        Err(rejection) => return rejection.into(),
    };
    let image = match knowledge_images::Image::new(input.image) {
        Ok(image) => image,
        Err(error) => return issue(error),
    };
    match image.insert(&app.db).await {
        Ok(id) => pb(StatusCode::OK, wire::KnowledgeImage { id: id.to_string() }),
        Err(error) => issue(error),
    }
}
pub async fn skill_text(app: &App, name: &str) -> Result<String, String> {
    let row=sqlx::query("SELECT * FROM runtime_skills WHERE (id=$1 OR lower(name)=lower($1)) AND deleted_at IS NULL ORDER BY builtin DESC,created_at LIMIT 1").bind(name).fetch_optional(&app.db).await.map_err(|e|e.to_string())?.ok_or("Skill不存在")?;
    let row = skill_resources::publish(&app.db, &app.root, &row.get::<String, _>("id")).await?;
    let item = skill(&row);
    Ok(format!(
        "Skill id={}，name={}，enabled={}，content_version={}\n\n{}\n\n资源目录：/skills/{}（只读，可通过workspace_run读取或在隔离环境执行；停用的Skill仅供管理，不应作为当前任务指令）。",
        item.id, item.name, item.enabled, item.content_version, item.content, item.id
    ))
}
#[cfg(test)]
pub(crate) async fn invoke(
    app: &App,
    name: &str,
    args: &Value,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Value, String> {
    invoke_tool(app, name, args, cancel, None).await
}

pub(crate) async fn invoke_tool(
    app: &App,
    name: &str,
    args: &Value,
    cancel: &tokio_util::sync::CancellationToken,
    run: Option<Uuid>,
) -> Result<Value, String> {
    let string = |key: &str| args[key].as_str().unwrap_or("").to_string();
    match name {
        "skill_list" => {
            let list = skills(app).await?;
            Ok(json!(
                list.skills
                    .iter()
                    .map(|s| json!({"id":s.id,"name":s.name,"description":s.description,"enabled":s.enabled,"builtin":s.builtin,"content_version":s.content_version}))
                    .collect::<Vec<_>>()
            ))
        }
        "skill_install" => {
            let item = skill_import(app, &string("source"), "", "").await?;
            Ok(
                json!({"id":item.id,"name":item.name,"source":item.source,"installed":true,"enabled":item.enabled,"content_version":item.content_version}),
            )
        }
        "skill_create" => {
            let content = string("content");
            if content.trim().is_empty() {
                return Err("需要完整SKILL.md正文".into());
            }
            let name = string("name");
            let (declared, _) = validate_skill(&content, &name)?;
            if !name.is_empty() && name.to_lowercase() != declared.to_lowercase() {
                return Err("name必须与SKILL.md头部一致".into());
            }
            let item = skill_import(app, "", &content, &string("name")).await?;
            Ok(
                json!({"id":item.id,"name":item.name,"saved":true,"enabled":item.enabled,"content_version":item.content_version}),
            )
        }
        "skill_edit" => {
            let item = skill_edit(
                app,
                wire::SkillEditRequest {
                    id: string("id"),
                    content: string("content"),
                    expected_content_version: string("expected_content_version"),
                },
            )
            .await?;
            Ok(
                json!({"id":item.id,"name":item.name,"updated":true,"enabled":item.enabled,"content_version":item.content_version}),
            )
        }
        "skill_set_enabled" => {
            let enabled = args["enabled"].as_bool().ok_or("enabled必须为布尔值")?;
            let item = skill_set_enabled(app, &string("id"), enabled, false).await?;
            Ok(
                json!({"id":item.id,"name":item.name,"enabled":item.enabled,"content_version":item.content_version}),
            )
        }
        "knowledge_search" => {
            let query = string("query");
            let pattern = format!("%{}%", query.trim().chars().take(200).collect::<String>());
            let rows=sqlx::query("SELECT id,title,left(content,360) content,revision,related_ids,false trashed FROM knowledge_documents WHERE deleted_at IS NULL AND (title ILIKE $1 OR content ILIKE $1) ORDER BY (title ILIKE $1) DESC,updated_at DESC LIMIT 10").bind(pattern).fetch_all(&app.db).await.map_err(|e|e.to_string())?;
            Ok(json!(rows.iter().map(|row| {
                let doc = document(row);
                json!({"id":doc.id,"title":doc.title,"excerpt":doc.content,"revision":doc.revision,"related_ids":doc.related_ids})
            }).collect::<Vec<_>>()))
        }
        "knowledge_read" => {
            let item = knowledge_read(app, &string("id")).await?;
            Ok(
                json!({"id":item.id,"title":item.title,"content":item.content,"revision":item.revision,"related_ids":item.related_ids}),
            )
        }
        "knowledge_write" => {
            let image = if let Some(observation) = args["image_observation_id"].as_str() {
                let run = run.ok_or("截图需要关联真实任务")?;
                let alt = string("image_alt").chars().take(140).collect();
                Some((
                    knowledge_images::observation(app, run, observation).await?,
                    alt,
                ))
            } else {
                None
            };
            let related = args
                .get("related_ids")
                .map(|value| serde_json::from_value::<Vec<String>>(value.clone()))
                .transpose()
                .map_err(|_| "关联文档 ID 无效")?;
            let keep_links = related.is_none();
            let item = save_document(
                app,
                wire::KnowledgeSaveRequest {
                    id: string("id"),
                    title: string("title"),
                    content: string("content"),
                    expected_revision: args["revision"].as_u64().unwrap_or(0),
                    related_ids: related.unwrap_or_default(),
                },
                keep_links,
                image,
            )
            .await?;
            Ok(
                json!({"id":item.id,"revision":item.revision,"title":item.title,"related_ids":item.related_ids}),
            )
        }
        "knowledge_link" => {
            let item = knowledge_link(
                app,
                wire::KnowledgeLinkRequest {
                    id: string("id"),
                    target_id: string("target_id"),
                    present: args["present"].as_bool().ok_or("请指定是否关联")?,
                    expected_revision: args["revision"].as_u64().ok_or("请先读取文档版本")?,
                },
            )
            .await?;
            Ok(json!({"id":item.id,"revision":item.revision,"related_ids":item.related_ids}))
        }
        "workspace_list" => {
            let list = files(app, &string("path")).await?;
            Ok(json!(list.files.iter().map(|f|json!({"path":f.path,"directory":f.directory,"bytes":f.size_bytes,"download":format!("/api/workbench/file?path={}",f.path)})).collect::<Vec<_>>()))
        }
        "workspace_read" => {
            let file = safe_path(&workspace(app), &string("path"), false).await?;
            let metadata = tokio::fs::metadata(&file)
                .await
                .map_err(|e| e.to_string())?;
            if metadata.len() > 1_000_000 {
                return Err("文件超过1MB，请使用工作指令有界读取".into());
            }
            let text = tokio::fs::read_to_string(file)
                .await
                .map_err(|_| "这不是UTF-8文本，请用工作指令处理")?;
            Ok(json!({"content":text}))
        }
        "workspace_write" => {
            let content = string("content");
            if content.len() > 1_000_000 {
                return Err("内容超过1MB".into());
            }
            let path = string("path");
            let file = safe_path(&workspace(app), &path, true).await?;
            tokio::fs::write(file, content)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({"path":path,"download":format!("/api/workbench/file?path={path}")}))
        }
        "workspace_fetch" => {
            let url = string("url");
            let data = download(app, &url, 20_000_000).await?;
            let path = string("path");
            if path.is_empty() {
                Ok(
                    json!({"source":url,"content":String::from_utf8_lossy(&data).chars().take(24000).collect::<String>()}),
                )
            } else {
                let file = safe_path(&workspace(app), &path, true).await?;
                tokio::fs::write(file, &data)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(
                    json!({"path":path,"bytes":data.len(),"download":format!("/api/workbench/file?path={path}")}),
                )
            }
        }
        "workspace_run" => {
            let script = string("script");
            if script.is_empty() || script.len() > 16000 {
                return Err("工作指令为空或过长".into());
            }
            ensure_workspace(app).await?;
            let image = workspace_image();
            let (ok, output) = runtime(app, &image).execute(&script, cancel).await?;
            Ok(json!({"success":ok,"output":output,"environment":"isolated workspace"}))
        }
        _ => Err("未知工作工具".into()),
    }
}
pub async fn upload_file(
    State(app): State<Shared>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Err(e) = auth(&app, &headers) {
        return e.into();
    }
    let field = match multipart.next_field().await {
        Ok(Some(v)) => v,
        _ => return issue("请选择文件".into()),
    };
    let original = field.file_name().unwrap_or("file.bin").to_string();
    let name = FilePath::new(&original)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .filter(|c| *c != '\0' && *c != '/' && *c != '\\')
        .take(160)
        .collect::<String>();
    let bytes = match field.bytes().await {
        Ok(v) if v.len() <= 10_000_000 => v,
        _ => return issue("文件超过10MB".into()),
    };
    let relative = format!("uploads/{}-{}", Uuid::now_v7(), name);
    let path = match safe_path(&workspace(&app), &relative, true).await {
        Ok(v) => v,
        Err(e) => return issue(e),
    };
    if let Err(e) = tokio::fs::write(path, &bytes).await {
        return issue(e.to_string());
    }
    match files(&app, "uploads").await {
        Ok(v) => pb(StatusCode::OK, v),
        Err(e) => issue(e),
    }
}

/// Preserve manual notes outside the generated section and respect disabled/deleted skills.
pub(crate) async fn publish_learning(
    app: &App,
    name: &str,
    package: &str,
    body: &str,
) -> Result<(), String> {
    const START: &str = "<!-- aiphone:learning:start -->";
    const END: &str = "<!-- aiphone:learning:end -->";
    let block = format!("{START}\n{body}\n{END}");
    let existing=sqlx::query("SELECT *,deleted_at IS NOT NULL AS removed FROM runtime_skills WHERE name=$1 ORDER BY created_at LIMIT 1").bind(name).fetch_optional(&app.db).await.map_err(|e|e.to_string())?;
    if let Some(row) = existing {
        if row.get::<bool, _>("removed") {
            return Ok(());
        }
        let previous = skill(&row);
        let Some(start) = previous.content.find(START) else {
            return Err("同名Skill不含自动经验区域，保留原文，未覆盖".into());
        };
        let Some(end) = previous.content.find(END) else {
            return Err("经验Skill区域不完整，未覆盖手动修改".into());
        };
        let content = format!(
            "{}{}{}",
            &previous.content[..start],
            block,
            &previous.content[end + END.len()..]
        );
        skill_edit(
            app,
            wire::SkillEditRequest {
                id: previous.id,
                content,
                expected_content_version: previous.content_version,
            },
        )
        .await?;
    } else {
        let content = format!(
            "---\nname: {name}\ndescription: 使用 {package} 时复用经真实操作与独立审计支持的经验，包含限制及失败情况。\n---\n\n{block}\n"
        );
        skill_import(app, "", &content, name).await?;
    }
    Ok(())
}
