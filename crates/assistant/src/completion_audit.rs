use crate::*;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::HashMap;
use std::ops::Range;

pub struct AuditResult {
    pub approved: bool,
    pub feedback: String,
    experience: Option<Value>,
    knowledge: Option<Value>,
}

type LearningEvidence = std::collections::BTreeMap<String, crate::app_learning::Metadata>;

fn learning_schema(evidence: &LearningEvidence) -> Option<Value> {
    if evidence.is_empty() {
        return None;
    }
    let mut schema = crate::tool_catalog::experience_schema();
    let properties = schema["properties"]
        .as_object_mut()
        .expect("experience schema");
    let mut references = properties
        .remove("observation_ids")
        .expect("experience references");
    references["items"]["enum"] = json!(evidence.keys().collect::<Vec<_>>());
    references["description"] = json!(
        "引用tool_evidence中的evidence_id，所有事件的learning_app须等于app_id；不是观察ID。由后端绑定真实观察，不自行编造ID。"
    );
    properties.insert("evidence_ids".into(), references);
    properties.get_mut("app_id").unwrap()["enum"] = json!(
        evidence
            .values()
            .map(|m| m.app_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
    );
    for field in schema["required"].as_array_mut().unwrap() {
        if field == "observation_ids" {
            *field = json!("evidence_ids");
        }
    }
    Some(schema)
}

fn learning_candidate(candidate: &Value, evidence: &LearningEvidence) -> Result<Value, String> {
    let app_id = candidate["app_id"].as_str().ok_or("经验缺少应用标识")?;
    let ids: Vec<String> = serde_json::from_value(candidate["evidence_ids"].clone())
        .map_err(|_| "经验需要本轮核验事件ID")?;
    let mut observations = Vec::with_capacity(ids.len());
    for id in ids {
        let actual = evidence.get(&id).ok_or("经验引用了本轮核验以外的事件")?;
        if actual.app_id != app_id {
            return Err("经验引用的事件不属于该应用".into());
        }
        observations.push(actual.observation_id.clone());
    }
    let mut value = candidate.clone();
    let fields = value.as_object_mut().ok_or("经验格式无效")?;
    fields.remove("evidence_ids");
    fields.insert("observation_ids".into(), json!(observations));
    Ok(value)
}

fn knowledge_schema(evidence: &LearningEvidence, illustrated: &[String]) -> Option<Value> {
    let app = &evidence.values().next()?.app_id;
    if !evidence.values().any(|item| &item.app_id != app) {
        return None;
    }
    let mut schema = json!({"type":"object","additionalProperties":false,
        "description":"可复用的跨应用知识，不记录单个应用的操作步骤。仅在当前真实证据足以支持结论时提供。",
        "properties":{
            "title":{"type":"string","maxLength":100},
            "summary":{"type":"string","maxLength":1200,"description":"结论和适用条件；不含私密内容。"},
            "evidence_ids":{"type":"array","minItems":2,"maxItems":8,"items":{"type":"string","enum":evidence.keys().collect::<Vec<_>>()}}
        },"required":["title","summary","evidence_ids"]});
    if !illustrated.is_empty() {
        schema["properties"]["image_evidence_id"] = json!({"type":"string","enum":illustrated,
            "description":"可选：同一结论所用的证据中，适合长期保留的真实截图。排除聊天、通知、账密和私人文件。"});
    }
    Some(schema)
}

struct KnowledgeCandidate<'a> {
    title: &'a str,
    summary: &'a str,
    observations: Vec<String>,
    image: Option<&'a str>,
}

fn knowledge_candidate<'a>(
    candidate: &'a Value,
    evidence: &'a LearningEvidence,
    illustrated: &[String],
) -> Option<KnowledgeCandidate<'a>> {
    let title = candidate["title"].as_str()?.trim();
    let summary = candidate["summary"].as_str()?.trim();
    let ids = candidate["evidence_ids"].as_array()?;
    if title.is_empty()
        || title.chars().count() > 100
        || summary.is_empty()
        || summary.chars().count() > 1200
        || !(2..=8).contains(&ids.len())
    {
        return None;
    }
    let mut apps = std::collections::BTreeSet::new();
    let mut observations = std::collections::BTreeSet::new();
    for id in ids {
        let item = evidence.get(id.as_str()?)?;
        apps.insert(&item.app_id);
        observations.insert(item.observation_id.clone());
    }
    let image = candidate["image_evidence_id"]
        .as_str()
        .filter(|id| {
            illustrated.iter().any(|known| known == id)
                && ids.iter().any(|value| value.as_str() == Some(*id))
        })
        .and_then(|id| evidence.get(id))
        .map(|frame| frame.observation_id.as_str());
    (apps.len() >= 2 && observations.len() == ids.len()).then(|| KnowledgeCandidate {
        title,
        summary,
        observations: observations.into_iter().collect(),
        image,
    })
}

fn single_desktop_input(evidence: &[Value]) -> bool {
    let mut inputs = 0;
    for receipt in evidence {
        match receipt["tool"].as_str() {
            Some("computer_type") if receipt["status"] == "completed" => {
                let observed = crate::worker::tool_value(&receipt["result"]).is_some_and(|value| {
                    value["success"] == true && value["observation_id"].is_string()
                });
                if !observed {
                    return false;
                }
                inputs += 1;
            }
            Some(
                "skill_read"
                | "computer_observe"
                | "computer_zoom"
                | "computer_find_text"
                | "computer_find_control"
                | "task_plan",
            ) => {}
            _ => return false,
        }
    }
    inputs == 1
}

struct SettingsTapAudit {
    source: Value,
    tap: usize,
    after: usize,
    visuals: Range<usize>,
}

fn single_settings_tap(
    evidence: &[Value],
    calls: &HashMap<Uuid, &Value>,
    post_visuals: Option<(Uuid, Range<usize>)>,
) -> Option<SettingsTapAudit> {
    let (visual_event, visuals) = post_visuals?;
    if evidence
        .iter()
        .any(|receipt| receipt["status"] != "completed")
    {
        return None;
    }
    let mut device_rows = evidence.iter().enumerate().filter(|(_, receipt)| {
        !matches!(receipt["tool"].as_str(), Some("skill_read" | "task_plan"))
    });
    let (_, before) = device_rows.next()?;
    let (tap, action) = device_rows.next()?;
    let (after, post) = device_rows.next()?;
    if device_rows.next().is_some()
        || before["tool"] != "phone_observe"
        || action["tool"] != "phone_tap_element"
        || post["tool"] != "phone_observe"
    {
        return None;
    }

    let result = crate::worker::tool_value(&action["result"])?;
    if result["success"] != true
        || result["not_executed"] == true
        || result["result_unknown"] == true
    {
        return None;
    }
    let action_event = Uuid::parse_str(action["evidence_id"].as_str()?).ok()?;
    let post_event = Uuid::parse_str(post["evidence_id"].as_str()?).ok()?;
    let action_call = calls.get(&action_event)?;
    let action_id = action_call["call_id"]
        .as_str()
        .filter(|id| !id.is_empty())?;
    if visual_event != post_event || calls.get(&post_event)?["local_after_action"] != action_id {
        return None;
    }

    let arguments: Value = serde_json::from_str(action_call["arguments"].as_str()?).ok()?;
    let source = crate::worker::tool_value(&before["result"])?;
    let destination = crate::worker::tool_value(&post["result"])?;
    let source_app = source["foreground_app"].as_str()?;
    let target_app = destination["foreground_app"].as_str()?;
    let device = source["device_id"].as_str().filter(|id| !id.is_empty())?;
    let source_observation = source["observation_id"]
        .as_str()
        .filter(|id| !id.is_empty())?;
    let target_observation = destination["observation_id"]
        .as_str()
        .filter(|id| !id.is_empty())?;
    if !source_app.starts_with("com.android.settings/")
        || !target_app.starts_with("com.android.settings/")
        || source_app == target_app
        || destination["device_id"] != device
        || source_observation == target_observation
        || arguments["observation_id"] != source_observation
    {
        return None;
    }
    let element_id = arguments["element_id"]
        .as_str()
        .filter(|id| !id.is_empty())?;
    let control = source["elements"].as_array()?.iter().find(|element| {
        element["id"] == element_id
            && element["source"] == "native"
            && element["clickable"] == true
            && element["text"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty())
    })?;
    let source = json!({"device_id":device,"observation_id":source_observation,"foreground_app":source_app,
        "target":{"id":element_id,"text":control["text"],"resource_id":control["resource_id"]}});
    Some(SettingsTapAudit {
        source: json!({"evidence_id":before["evidence_id"],"tool":"phone_observe","status":"completed",
            "result":[{"type":"input_text","text":source.to_string()}]}),
        tap,
        after,
        visuals,
    })
}

// A completion audit has no execution tools. Its input comes from this run's
// persisted tool ledger, never from historical assistant prose.
pub async fn verify(
    app: &App,
    run: Uuid,
    message: Uuid,
    context: &[Value],
    summary: &str,
    needs_input: bool,
    cancel: &CancellationToken,
) -> Result<AuditResult, String> {
    let request = (0..context.len()).rfind(|&index| crate::worker::is_task_input(context, index));
    let latest_input = request.unwrap_or(0);
    let current_calls: HashMap<Uuid, &Value> = context
        .iter()
        .skip(latest_input + 1)
        .filter(|item| item["type"] == "function_call")
        .filter_map(|item| {
            Some((
                Uuid::parse_str(item["local_action_id"].as_str()?).ok()?,
                item,
            ))
        })
        .collect();
    let ids: Vec<_> = current_calls.keys().copied().collect();
    let rows = sqlx::query("SELECT e.id,e.title,e.status,e.result,r.origin='user' AS user_run FROM harness_events e JOIN runs r ON r.id=e.run_id WHERE e.run_id=$1 AND e.id=ANY($2) AND e.kind='tool' AND e.result IS NOT NULL ORDER BY e.created_at DESC,e.id DESC LIMIT 64")
        .bind(run).bind(&ids).fetch_all(&app.db).await.map_err(|e| e.to_string())?;
    if !needs_input
        && !rows.iter().any(|row| {
            row.get::<&str, _>("status") == "completed"
                && current_calls[&row.get::<Uuid, _>("id")]["name"] != "skill_read"
        })
    {
        return Err("本轮没有已确认的执行回执，无法核验完成。".into());
    }
    let event = Uuid::now_v7();
    sqlx::query("INSERT INTO harness_events(id,run_id,message_id,kind,title,content) VALUES($1,$2,$3,'audit','结果核验','核对本轮工具记录与完成声明')")
        .bind(event).bind(run).bind(message).execute(&app.db).await.map_err(|e| e.to_string())?;
    let mut image_budget = 8usize;
    let mut visual_inputs = Vec::new();
    let mut input_visuals = None;
    let mut post_visuals = None;
    let mut evidence = Vec::new();
    let mut learning_evidence = LearningEvidence::new();
    let mut illustrated = Vec::new();
    for row in &rows {
        let row_id = row.get::<Uuid, _>("id");
        let id = row_id.to_string();
        let call = current_calls[&row_id];
        let mut result = row.get::<Option<Value>, _>("result").unwrap_or(Value::Null);
        let name = call["name"].as_str().unwrap_or("");
        if name == "skill_read" {
            // The skill text guides planning; only its read status belongs in the audit.
            result = Value::Null;
        }
        let observation = (row.get::<String, _>("status") == "completed"
            && crate::app_learning::observation_tool(name))
        .then(|| crate::app_learning::metadata(&result))
        .flatten();
        if let Some(observation) = observation {
            learning_evidence.insert(id.clone(), observation);
        }
        limit_result(&mut result, &mut image_budget);
        let first_visual = visual_inputs.len();
        extract_images(&mut result, &id, &mut visual_inputs);
        if first_visual < visual_inputs.len() && learning_evidence.contains_key(&id) {
            illustrated.push(id.clone());
        }
        if name == "computer_type" && first_visual < visual_inputs.len() {
            input_visuals = Some(first_visual..visual_inputs.len());
        }
        if name == "phone_observe"
            && call["local_after_action"].is_string()
            && visual_inputs.len() == first_visual + 2
            && visual_inputs[first_visual + 1]["image_url"]
                .as_str()
                .and_then(|url| url.strip_prefix("data:image/png;base64,"))
                .is_some_and(|png| !png.is_empty())
        {
            post_visuals = Some((row_id, first_visual..visual_inputs.len()));
        }
        evidence.push(json!({"evidence_id":id,"tool":name,"learning_app":learning_evidence.get(&id).map(|m|m.app_id.as_str()),
            "input":call["arguments"].as_str().unwrap_or(""),
            "title":row.get::<String,_>("title"),"status":row.get::<String,_>("status"),"result":result}));
    }
    evidence.reverse();
    let request_text = request
        .and_then(|index| context[index]["content"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let (handoff_receipts, notifications, learning) = tokio::try_join!(
        sqlx::query_scalar::<_, Value>("SELECT e.result FROM harness_events e JOIN runs r ON r.id=e.run_id WHERE e.run_id=$1 AND e.kind='summary' AND e.title='设备协作' AND e.status='completed' AND e.result IS NOT NULL AND e.created_at>=COALESCE(r.input_at,r.created_at) ORDER BY e.created_at")
            .bind(run).fetch_all(&app.db),
        sqlx::query_scalar::<_, Value>("SELECT jsonb_build_object('device_id',n.device_id,'app',n.app_name,'title',n.title,'text',n.body) FROM phone_notifications n JOIN runs r ON r.id=n.run_id WHERE n.run_id=$1 AND NOT r.background_read_only AND n.status IN ('assigned','attending','pending-summary') ORDER BY n.received_at LIMIT 16")
            .bind(run).fetch_all(&app.db),
        sqlx::query_scalar::<_, Value>("SELECT jsonb_build_object('app',app_id,'title',title,'procedure',procedure,'outcome',outcome,'limitations',limitations,'observations',observation_ids) FROM app_experiences WHERE run_id=$1 AND superseded_at IS NULL ORDER BY created_at LIMIT 8")
            .bind(run).fetch_all(&app.db),
    ).map_err(|e| e.to_string())?;
    let compact_allowed = !needs_input
        && handoff_receipts.is_empty()
        && notifications.is_empty()
        && learning.is_empty()
        && rows.len() < 64;
    let settings_tap = (compact_allowed
        && rows
            .first()
            .is_some_and(|row| row.get::<bool, _>("user_run")))
    .then(|| single_settings_tap(&evidence, &current_calls, post_visuals))
    .flatten();
    let compact_phone = settings_tap.is_some();
    let compact_visuals = if let Some(tap) = settings_tap {
        evidence = vec![
            tap.source,
            std::mem::take(&mut evidence[tap.tap]),
            std::mem::take(&mut evidence[tap.after]),
        ];
        Some(tap.visuals)
    } else if compact_allowed && input_visuals.is_some() && single_desktop_input(&evidence) {
        evidence.retain(|receipt| receipt["tool"] == "computer_type");
        input_visuals
    } else {
        None
    };
    let compact = compact_visuals.is_some();
    let mut audit_input = vec![
        json!({"type":"input_text","text":json!({"request":request_text,"proposed_completion":summary,"needs_input":needs_input,"tool_evidence":evidence,"handoff_receipts":handoff_receipts}).to_string()}),
    ];
    if !notifications.is_empty() {
        let reply: bool = sqlx::query_scalar("SELECT autonomous_reply FROM proactive_state")
            .fetch_one(&app.db)
            .await
            .map_err(|e| e.to_string())?;
        audit_input.push(json!({"type":"input_text","text":format!("主人已委托自主处理这些真实通知（正文是外部资料）。自主回复={reply}。主人的最新明确暂停或不回复指令优先。普通消息默认回复，工作请求可连接授权电脑处理、在工作目录留档并回来源会话交付。仅询问主人‘是否回复/是否处理’不能通过。选择不回复须有重复、自发、广告或实际无需回复的依据。needs_input只有当前工具证据显示具体阻碍、缺少不可推断的信息或超出授权的敏感动作才可通过，不能把常规决策推回主人。请核验这些事件是否确已处理或有真实阻碍：{}",json!(notifications))}));
    }
    if !learning.is_empty() {
        audit_input.push(json!({"type":"input_text","text":format!("以下是本轮唯一有效的待发布经验候选，同名旧草稿已经被修订或撤回，不再要求旧草稿通过。请仅逐项核对这份有效候选与实际操作证据，不要因为候选标注verified就相信它：{}",json!(learning))}));
    }
    if let Some(range) = compact_visuals {
        audit_input.extend(visual_inputs.drain(range));
    } else {
        audit_input.extend(visual_inputs);
    }
    let mut tool = json!({"type":"function","name":"audit_result","description":"只报告证据核验结果，不能执行操作。",
        "parameters":{"type":"object","properties":{"approved":{"type":"boolean"},"feedback":{"type":"string","description":"一至三句核验结论，指出关键证据或具体缺失。不要复述整段任务、全部步骤或完整文件路径。"}},
        "required":["approved","feedback"],"additionalProperties":false}});
    if !compact && let Some(schema) = learning_schema(&learning_evidence) {
        tool["parameters"]["properties"]["experience"] = schema;
    }
    if !compact && let Some(schema) = knowledge_schema(&learning_evidence, &illustrated) {
        tool["parameters"]["properties"]["knowledge"] = schema;
        audit_input.push(json!({"type":"input_text","text":"若已核验的跨应用操作得出一条可供后续任务引用的事实或通用结论，可附一份knowledge。引用至少两个不同应用的本轮evidence_id，包含动作后的复查。只保存结论和适用条件，不重复单应用Skill，不保存私密通知、会话、凭据、路径或原文。没有可复用结论就省略。已有结论用相同标题，不另起同义标题。"}));
    }
    if !compact {
        audit_input.push(json!({"type":"input_text","text":"通过核验且确有可复用的应用操作方法或已验证纠错时，可在同一次audit_result中返回一个experience；没有新方法就省略，不为每轮凑心得。evidence_ids仅引用tool_evidence中的evidence_id，所引事件的learning_app都须等于app_id；不要填写嵌套观察ID或跨应用事件。verified至少两次且有动作后复查。沿用已有同义方法的标题以修订整合，不重复创建；只记操作方法与适用边界，不含联系人、会话内容、账号、凭据或私人文件名。资料不足不提炼。用户任务结果独立于学习候选，不额外调用模型、不增加操作回合、不向用户输出学习报告。"}));
    }
    let instructions = if compact_phone {
        "核对单次 Android 设置页面跳转。检查真实点击回执、点击前的控件记录和点击后的同设备画面与可见控件。只有点击目标和实际到达页面都符合用户要求，且完成声明中的事实均有画面或控件证据时，才可 approved=true。点击成功本身不证明目标页面已到达。只调用 audit_result，用一句话报告证据或阻碍。"
    } else if compact {
        "核对单次电脑输入。以执行器的输入回执与动作后真实画面核对用户要求的内容、目标窗口与完成声明。只有画面或可靠回读确认实际内容才可通过；键盘派发本身不证明文本已进入。仅调用 audit_result，用一句话报告证据或阻碍。"
    } else {
        "你是独立的执行结果审计器。只依据给定本轮真实工具记录、截图和用户目标核验，不相信助手的完成声明或历史对话。设备协作回执可核对文件传输；外发结果须由来源应用的实际操作记录支持。如果记录包含app_experience_save，必须同时核对其步骤、成功结论和失败边界能否由该应用真实操作前后画面支持；不能把打开页面当作掌握功能，不得批准猜测为通用经验。严格检查：是否实际执行了声称的动作、点击/阅读的数量是否与记录相符、所述页面/文章事实能否从截图或UI文本读到、用户目标是否已完成。读Skill不等于操作，成功点击也不等于读到文章；标题不能证明文章或评论内容。若没有相应证据、只有计划/推测、尚有常规步骤未执行，approved必须false，并具体列出缺失证据和应继续的步骤。你没有执行工具，不能补造证据。只有确实得到当前记录支持的完成结果才可approved=true。输出简短可公开核验的结论，不输出隐含推理过程、凭据或无关内容。微信的通知预览不代表完整会话，必须核对对象。己方绿色气泡只证明本机已发出，不能证明对方已收到或已读；账号受限或验证页卡住不能称好友申请成功。好友关系当前已成立可以用当前资料页和已添加状态核验，不强求重做接受动作。较早截图已归档时若现存证据不足，应要求重新观察；不能凭归档标记猜图片内容。"
    };
    let body = json!({"input":[
        {"role":"system","content":[{"type":"input_text","text":instructions}]},
        {"role":"user","content":audit_input}
    ],"tools":[tool],"tool_choice":{"type":"function","name":"audit_result"},"parallel_tool_calls":false,
        "reasoning":{"effort":"none"},"max_output_tokens":if compact {192} else {1200},"stream":false,"store":false});
    let mut timing = crate::model_timing::Timing::default();
    let audit = async {
        let config = crate::settings::for_run(app, run).await?;
        let client = app.settings.http_client(&config.proxy_url)?;
        let output = tokio::time::timeout(
            Duration::from_secs(60),
            crate::model_adapter::request_tracked(&config, &client, body, cancel, &mut timing),
        )
        .await
        .map_err(|_| "结果核验超时；尚未确认完成")??;
        parse_result(&json!({"output":output}))
    };
    let result =
        tokio::select! {_ = cancel.cancelled() => Err("canceled".into()), result = audit => result};
    if !compact
        && let Ok(report) = &result
        && let Some(candidate) = report.experience.as_ref().filter(|_| !needs_input)
    {
        match learning_candidate(candidate, &learning_evidence) {
            Ok(candidate) => {
                retain_learning(app, run, false, Some(&candidate)).await;
            }
            Err(error) => eprintln!(
                "Optional app learning was not retained; user task result preserved: {error}"
            ),
        }
    }
    let (status, content) = match &result {
        Ok(result) => (
            if result.approved {
                "completed"
            } else {
                "failed"
            },
            result.feedback.clone(),
        ),
        Err(error) => (
            if error == "canceled" {
                "canceled"
            } else {
                "failed"
            },
            error.clone(),
        ),
    };
    let mut audit_timing = timing.snapshot();
    audit_timing["compact"] = json!(compact);
    sqlx::query(
        "UPDATE harness_events SET status=$2,content=$3,result=$4,finished_at=now() WHERE id=$1",
    )
    .bind(event)
    .bind(status)
    .bind(content)
    .bind(audit_timing)
    .execute(&app.db)
    .await
    .map_err(|e| e.to_string())?;
    if !compact
        && !needs_input
        && let Ok(report) = &result
        && report.approved
        && let Some(note) = report
            .knowledge
            .as_ref()
            .and_then(|candidate| knowledge_candidate(candidate, &learning_evidence, &illustrated))
        && let Err(error) = crate::workbench::publish_learned_knowledge(
            app,
            run,
            event,
            note.title,
            note.summary,
            &note.observations,
            note.image,
        )
        .await
    {
        eprintln!("Optional knowledge was not retained: {error}");
    }
    result
}

// Learning shares the completed audit's evidence and model call. A rejected
// optional candidate never turns a valid user result into another tool loop.
pub(crate) async fn retain_learning(
    app: &App,
    run: Uuid,
    needs_input: bool,
    candidate: Option<&Value>,
) -> bool {
    let Some(candidate) = candidate.filter(|_| !needs_input) else {
        return false;
    };
    match crate::app_learning::save(app, run, candidate).await {
        Ok(_) => true,
        Err(error) => {
            eprintln!(
                "Optional app learning was not retained; user task result preserved: {}",
                app.settings.redact(&error)
            );
            false
        }
    }
}

// History is evidence about previous work, never proof of a new action.
pub async fn history(app: &App, run: Uuid, limit: usize) -> Result<Value, String> {
    let rows=sqlx::query("SELECT r.id,r.status,r.created_at,m.text FROM runs r JOIN messages m ON m.id=r.assistant_message_id WHERE r.conversation_id=(SELECT conversation_id FROM runs WHERE id=$1) ORDER BY r.created_at DESC,r.id DESC LIMIT $2")
        .bind(run).bind(limit.clamp(1,8) as i64).fetch_all(&app.db).await.map_err(|e|e.to_string())?;
    let mut history = Vec::new();
    for row in rows {
        let id: Uuid = row.get("id");
        let events=sqlx::query("SELECT id,title,status,content FROM harness_events WHERE run_id=$1 AND kind IN ('tool','audit') ORDER BY created_at,id LIMIT 100")
            .bind(id).fetch_all(&app.db).await.map_err(|e|e.to_string())?;
        history.push(json!({"run_id":id.to_string(),"status":row.get::<String,_>("status"),
            "assistant_claim_not_evidence":row.get::<String,_>("text").chars().take(1600).collect::<String>(),
            "actual_ledger":events.iter().map(|event|json!({"id":event.get::<Uuid,_>("id").to_string(),"title":event.get::<String,_>("title"),"status":event.get::<String,_>("status"),"content":app.settings.redact(&event.get::<String,_>("content")).chars().take(4000).collect::<String>()})).collect::<Vec<_>>() }));
    }
    Ok(json!({"note":"这是旧轮次真实账本，仅用于回答质疑，不可用作本轮执行证据","runs":history}))
}

fn parse_result(value: &Value) -> Result<AuditResult, String> {
    let mut calls = value["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call");
    let args = calls
        .next()
        .filter(|item| item["name"] == "audit_result")
        .and_then(|item| item["arguments"].as_str())
        .ok_or("结果核验没有返回有效结论")?;
    if calls.next().is_some() {
        return Err("结果核验必须返回唯一结论".into());
    }
    let report: Value = serde_json::from_str(args).map_err(|_| "结果核验没有返回有效结论")?;
    let approved = report["approved"].as_bool().ok_or("结果核验缺少证据判定")?;
    let feedback = report["feedback"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("结果核验缺少说明")?;
    Ok(AuditResult {
        approved,
        feedback: feedback.chars().take(4000).collect(),
        experience: report
            .get("experience")
            .filter(|v| approved && v.is_object())
            .cloned(),
        knowledge: report
            .get("knowledge")
            .filter(|v| approved && v.is_object())
            .cloned(),
    })
}

fn extract_images(value: &mut Value, id: &str, inputs: &mut Vec<Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                extract_images(item, id, inputs);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("input_image") {
                inputs.push(json!({"type":"input_text","text":format!("真实截图证据 {id}")}));
                inputs.push(std::mem::replace(
                    value,
                    json!({"type":"input_text","text":format!("真实截图证据 {id} 见附图")}),
                ));
            } else {
                for item in object.values_mut() {
                    extract_images(item, id, inputs);
                }
            }
        }
        _ => {}
    }
}

fn limit_result(value: &mut Value, images: &mut usize) {
    match value {
        Value::Array(items) => {
            for item in items.iter_mut().rev() {
                limit_result(item, images);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("input_image") {
                if *images == 0 {
                    *value = json!({"type":"input_text","text":"较早截图已归档，不能据此猜测内容"});
                    return;
                }
                *images -= 1;
            }
            if let Some(Value::String(text)) = object.get_mut("text")
                && text.len() > 24000
            {
                *text = text.chars().take(24000).collect();
            }
            for (key, item) in object {
                if key != "image_url" {
                    limit_result(item, images);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_review_requires_one_confirmed_input_and_its_observation() {
        let observation = json!({"tool":"computer_observe","status":"completed"});
        let zoom = json!({"tool":"computer_zoom","status":"completed"});
        let mut input = json!({"tool":"computer_type","status":"completed","result":[
            {"type":"input_text","text":json!({"success":true,"observation_id":"fresh"}).to_string()}
        ]});
        assert!(single_desktop_input(&[
            observation.clone(),
            input.clone(),
            zoom
        ]));
        assert!(!single_desktop_input(&[input.clone(), input.clone()]));
        input["status"] = json!("failed");
        assert!(!single_desktop_input(&[observation, input]));
    }

    #[test]
    fn settings_short_review_requires_its_own_tap_and_post_action_image() {
        let before = Uuid::new_v4();
        let action = Uuid::new_v4();
        let after = Uuid::new_v4();
        let receipt = |id, tool, result: Value| {
            json!({"evidence_id":id,"tool":tool,"status":"completed",
            "result":[{"type":"input_text","text":result.to_string()}]})
        };
        let source = json!({"device_id":"phone-local-01","observation_id":"home",
            "foreground_app":"com.android.settings/.Settings","elements":[
                {"id":"n1","source":"native","clickable":true,"text":"Battery","resource_id":"android:id/title"},
                {"id":"n2","source":"native","clickable":true,"text":"Network"}]});
        let destination = json!({"device_id":"phone-local-01","observation_id":"details",
            "foreground_app":"com.android.settings/.SubSettings"});
        let mut evidence = vec![
            receipt(before, "phone_observe", source),
            receipt(
                action,
                "phone_tap_element",
                json!({"success":true,"output":"tap"}),
            ),
            receipt(after, "phone_observe", destination.clone()),
        ];
        let tap_call = json!({"call_id":"tap","arguments":"{\"observation_id\":\"home\",\"element_id\":\"n1\"}"});
        let post_call = json!({"local_after_action":"tap"});
        let unrelated_call = json!({"local_after_action":"other-tap"});
        let calls = HashMap::from([(action, &tap_call), (after, &post_call)]);
        let images = Some((after, 0..2));
        let short = single_settings_tap(&evidence, &calls, images.clone()).unwrap();
        assert_eq!(short.visuals, 0..2);
        let target = crate::worker::tool_value(&short.source["result"]).unwrap();
        assert_eq!(target["target"]["text"], "Battery");
        assert!(target.get("elements").is_none());

        assert!(single_settings_tap(&evidence, &calls, None).is_none());
        let unlinked = HashMap::from([(action, &tap_call), (after, &unrelated_call)]);
        assert!(single_settings_tap(&evidence, &unlinked, images.clone()).is_none());
        evidence[2] = receipt(
            after,
            "phone_observe",
            json!({"device_id":"another-phone",
            "observation_id":"details","foreground_app":"com.android.settings/.SubSettings"}),
        );
        assert!(single_settings_tap(&evidence, &calls, images.clone()).is_none());
        evidence[2] = receipt(after, "phone_observe", destination);
        evidence.push(evidence[1].clone());
        assert!(single_settings_tap(&evidence, &calls, images).is_none());
    }

    #[test]
    fn audit_learning_references_only_the_exposed_event_namespace() {
        let evidence = LearningEvidence::from([(
            "event-id".into(),
            crate::app_learning::Metadata {
                app_id: "linux:libreoffice-draw".into(),
                device_id: "desktop".into(),
                observation_id: "frame-id".into(),
            },
        )]);
        let schema = learning_schema(&evidence).unwrap();
        assert!(schema["properties"].get("observation_ids").is_none());
        assert_eq!(
            schema["properties"]["evidence_ids"]["items"]["enum"],
            json!(["event-id"])
        );
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "evidence_ids")
        );
        // Execution tools retain their observation-ID contract.
        assert!(
            crate::tool_catalog::experience_schema()["properties"]
                .get("observation_ids")
                .is_some()
        );
        assert!(learning_schema(&LearningEvidence::new()).is_none());
    }
    #[test]
    fn audit_learning_binds_current_app_events_to_actual_observations() {
        let evidence = LearningEvidence::from([
            (
                "event-a".into(),
                crate::app_learning::Metadata {
                    app_id: "linux:libreoffice-draw".into(),
                    device_id: "desktop".into(),
                    observation_id: "frame-a".into(),
                },
            ),
            (
                "event-b".into(),
                crate::app_learning::Metadata {
                    app_id: "linux:libreoffice-calc".into(),
                    device_id: "desktop".into(),
                    observation_id: "frame-b".into(),
                },
            ),
        ]);
        let candidate = json!({"app_id":"linux:libreoffice-draw","title":"draw method","evidence_ids":["event-a"]});
        let actual = learning_candidate(&candidate, &evidence).unwrap();
        assert_eq!(actual["observation_ids"], json!(["frame-a"]));
        assert!(actual.get("evidence_ids").is_none());
        for ids in [
            json!(["frame-a"]),
            json!(["unknown-event"]),
            json!(["event-a", "event-b"]),
            json!(null),
        ] {
            let mut invalid = candidate.clone();
            invalid["evidence_ids"] = ids;
            assert!(learning_candidate(&invalid, &evidence).is_err());
        }
        assert!(!crate::app_learning::observation_tool("host_exec"));
        assert!(!crate::app_learning::observation_tool("workspace_read"));
    }
    #[test]
    fn audit_cannot_default_to_approved_on_missing_or_malformed_result() {
        assert!(parse_result(&json!({"output":[]})).is_err());
        assert!(parse_result(&json!({"output":[{"type":"function_call","name":"audit_result","arguments":"{\"feedback\":\"done\"}"}]})).is_err());
    }
    #[test]
    fn successful_audit_retains_optional_experience_without_requiring_another_model_turn() {
        let candidate = json!({"app_id":"com.example.reader","title":"搜索","procedure":"点击当前可见搜索入口，输入后核对实际列表。","outcome":"verified","limitations":"仅限已观察版本","observation_ids":["before","after"]});
        let result=parse_result(&json!({"output":[{"type":"function_call","name":"audit_result","arguments":json!({"approved":true,"feedback":"界面与记录吻合","experience":candidate}).to_string()}]})).unwrap();
        assert!(result.approved);
        assert_eq!(result.experience, Some(candidate));
    }
    #[test]
    fn rejected_or_absent_experience_does_not_create_learning_or_reject_a_valid_task() {
        for (approved, experience) in [
            (false, json!({"app_id":"com.example.fake"})),
            (true, Value::Null),
            (true, json!("invalid")),
        ] {
            let result=parse_result(&json!({"output":[{"type":"function_call","name":"audit_result","arguments":json!({"approved":approved,"feedback":"核验结论","experience":experience}).to_string()}]})).unwrap();
            assert_eq!(result.approved, approved);
            assert!(result.experience.is_none());
        }
    }
    #[test]
    fn evidence_retains_only_bounded_recent_images() {
        let mut result = json!([{"type":"input_image","image_url":"old"},{"type":"input_image","image_url":"new"}]);
        limit_result(&mut result, &mut 1);
        assert!(result[0].get("image_url").is_none());
        assert_eq!(result[1]["image_url"], "new");
    }
    #[tokio::test]
    #[ignore = "real disposable PostgreSQL and loopback audit provider; no external model or device calls"]
    async fn audit_event_references_save_promote_and_recall_only_real_current_app_evidence() {
        use axum::{Json, Router, routing::post};
        async fn scenario(app: Shared) {
            let reply = Arc::new(std::sync::Mutex::new(Value::Null));
            let response = reply.clone();
            let (sent, mut received) = tokio::sync::mpsc::channel(4);
            let router = Router::new().route("/responses",post(move |Json(body):Json<Value>| {
                let sent=sent.clone();let response=response.clone();
                async move {
                    sent.send(body).await.unwrap();
                    let args=response.lock().unwrap().clone();
                    let event=json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","name":"audit_result","arguments":args.to_string()}]}});
                    ([("content-type","text/event-stream")],format!("data: {event}\n\n"))
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            struct Server(tokio::task::JoinHandle<()>);
            impl Drop for Server {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _server = Server(tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap()
            }));
            app.settings.test_revision(crate::settings::ModelConfig {
                api_key: String::new(),
                base_url: format!("http://{address}"),
                model: "audit-fixture".into(),
                provider: "compatible".into(),
                protocol: "responses".into(),
                proxy_url: String::new(),
                verified_at_ms: 1,
            });
            crate::registry::initialize(&app.db, &app.settings)
                .await
                .unwrap();
            let conversation: Uuid =
                sqlx::query_scalar("SELECT id FROM conversations WHERE owner_id='local-user'")
                    .fetch_one(&app.db)
                    .await
                    .unwrap();
            let run = Uuid::now_v7();
            let message = Uuid::now_v7();
            sqlx::query("INSERT INTO messages(id,conversation_id,role) VALUES($1,$2,'assistant')")
                .bind(message)
                .bind(conversation)
                .execute(&app.db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO runs(id,conversation_id,assistant_message_id,request_id,request_fingerprint,status) VALUES($1,$2,$3,$1,'fixture','running')").bind(run).bind(conversation).bind(message).execute(&app.db).await.unwrap();
            let ids = [
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
            ];
            let mut context = vec![
                json!({"role":"user","content":[{"type":"input_text","text":"Read the current app"}]}),
            ];
            for (i, id) in ids.iter().enumerate() {
                let tool = if i == 3 {
                    "workspace_read"
                } else {
                    "phone_observe"
                };
                let package = if i == 2 {
                    "com.example.other"
                } else {
                    "com.example.reader"
                };
                let mut output = json!([{"type":"input_text","text":json!({"success":true,"device_id":"phone-local-01","observation_id":format!("frame-{i}"),"foreground_app":package}).to_string()}]);
                if i == 2 {
                    let png = include_bytes!("../fixtures/vision-probe.png");
                    let encoded =
                        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png);
                    output.as_array_mut().unwrap().push(json!({"type":"input_image","image_url":format!("data:image/png;base64,{encoded}")}));
                }
                sqlx::query("INSERT INTO harness_events(id,run_id,message_id,kind,title,status,result) VALUES($1,$2,$3,'tool','fixture','completed',$4)").bind(id).bind(run).bind(message).bind(&output).execute(&app.db).await.unwrap();
                context.push(json!({"type":"function_call","name":tool,"local_action_id":id.to_string(),"arguments":"{}"}));
                if crate::app_learning::observation_tool(tool) {
                    crate::app_learning::observe(&app, run, *id, i > 0, &output)
                        .await
                        .unwrap();
                }
            }
            let other_run = Uuid::now_v7();
            let other_message = Uuid::now_v7();
            sqlx::query("INSERT INTO messages(id,conversation_id,role) VALUES($1,$2,'assistant')")
                .bind(other_message)
                .bind(conversation)
                .execute(&app.db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO runs(id,conversation_id,assistant_message_id,request_id,request_fingerprint,status) VALUES($1,$2,$3,$1,'fixture','canceled')")
                .bind(other_run).bind(conversation).bind(other_message).execute(&app.db).await.unwrap();
            let wrong_run = verify(
                &app,
                other_run,
                other_message,
                &context,
                "Read the app",
                false,
                &CancellationToken::new(),
            )
            .await;
            assert!(
                matches!(wrong_run, Err(error) if error == "本轮没有已确认的执行回执，无法核验完成。")
            );
            assert!(
                received.try_recv().is_err(),
                "another run's receipts reached the model"
            );
            let mut candidate = json!({"app_id":"com.example.reader","title":"Open the search field","procedure":"Click the visible search entry and verify the actual input field before typing.","outcome":"verified","limitations":"Only the observed application version."});
            for (references, saved, note_references, approved, notes) in [
                (
                    json!([ids[0].to_string(), ids[2].to_string()]),
                    0i64,
                    [ids[0], ids[1]],
                    true,
                    0i64,
                ),
                (
                    json!([ids[0].to_string(), "unknown-event"]),
                    0,
                    [ids[0], ids[3]],
                    true,
                    0,
                ),
                (
                    json!([ids[0].to_string(), ids[3].to_string()]),
                    0,
                    [ids[0], ids[2]],
                    false,
                    0,
                ),
                (
                    json!([ids[0].to_string(), ids[3].to_string()]),
                    0,
                    [ids[0], ids[2]],
                    true,
                    1,
                ),
                (
                    json!([ids[0].to_string(), ids[1].to_string()]),
                    1,
                    [ids[0], ids[2]],
                    true,
                    1,
                ),
            ] {
                candidate["evidence_ids"] = references;
                *reply.lock().unwrap() = json!({"approved":approved,"feedback":"Fixture task evidence","experience":candidate,"knowledge":{
                    "title":"Check import results in both applications",
                    "summary":"Verify the actual result in the receiving application instead of assuming a handoff is complete.",
                    "evidence_ids":note_references,
                    "image_evidence_id":ids[2],
                }});
                assert_eq!(
                    verify(
                        &app,
                        run,
                        message,
                        &context,
                        "Fixture result",
                        false,
                        &CancellationToken::new()
                    )
                    .await
                    .unwrap()
                    .approved,
                    approved
                );
                let request = received.recv().await.unwrap();
                let schema = &request["tools"][0]["parameters"]["properties"]["experience"];
                assert!(schema["properties"].get("observation_ids").is_none());
                let allowed = schema["properties"]["evidence_ids"]["items"]["enum"]
                    .as_array()
                    .unwrap();
                assert!(allowed.contains(&json!(ids[0].to_string())));
                assert!(
                    !allowed.contains(&json!(ids[3].to_string())),
                    "arbitrary workspace output cannot produce GUI learning evidence"
                );
                assert_eq!(
                    sqlx::query_scalar::<_, i64>(
                        "SELECT count(*) FROM app_experiences WHERE run_id=$1"
                    )
                    .bind(run)
                    .fetch_one(&app.db)
                    .await
                    .unwrap(),
                    saved
                );
                let knowledge = crate::workbench::invoke(
                    &app,
                    "knowledge_search",
                    &json!({"query":"Check import results"}),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
                assert_eq!(knowledge.as_array().unwrap().len(), notes as usize);
            }
            let (id, initial): (Uuid, i64) =
                sqlx::query_as("SELECT id,revision FROM knowledge_documents WHERE learned_key=$1")
                    .bind("check import results in both applications")
                    .fetch_one(&app.db)
                    .await
                    .unwrap();
            let image: Vec<u8> = sqlx::query_scalar("SELECT image FROM knowledge_images")
                .fetch_one(&app.db)
                .await
                .unwrap();
            assert_eq!(image, include_bytes!("../fixtures/vision-probe.png"));
            let observations = [String::from("frame-0"), String::from("frame-2")];
            assert!(
                crate::workbench::publish_learned_knowledge(
                    &app,
                    other_run,
                    Uuid::now_v7(),
                    "Check import results in both applications",
                    "A different run cannot attach this screenshot.",
                    &observations,
                    Some("frame-2"),
                )
                .await
                .is_err()
            );
            crate::workbench::publish_learned_knowledge(
                &app,
                run,
                Uuid::now_v7(),
                "Check import results in both applications",
                "Reopen the result in the receiving application to verify its format.",
                &observations,
                None,
            )
            .await
            .unwrap();
            let (content, revision): (String, i64) =
                sqlx::query_as("SELECT content,revision FROM knowledge_documents WHERE id=$1")
                    .bind(id)
                    .fetch_one(&app.db)
                    .await
                    .unwrap();
            assert!(content.contains("](/api/workbench/knowledge-image/"));
            assert!(content.contains("Reopen the result"));
            assert_eq!(revision, initial + 1);
            let related = crate::workbench::invoke_tool(
                &app,
                "knowledge_write",
                &json!({"title":"Inspect the receiving application","content":"Use the application UI to reopen the result.","related_ids":[id]}),
                &CancellationToken::new(),
                Some(run),
            )
            .await
            .unwrap();
            let linked = crate::workbench::invoke(
                &app,
                "knowledge_read",
                &json!({"id":id}),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(linked["related_ids"], json!([related["id"]]));
            let revised = crate::workbench::invoke_tool(
                &app,
                "knowledge_write",
                &json!({"id":related["id"],"revision":related["revision"],"title":"Inspect the receiving application","content":"Read the final result in the UI.","image_observation_id":"frame-2","image_alt":"Observed import"}),
                &CancellationToken::new(),
                Some(run),
            )
            .await
            .unwrap();
            assert_eq!(revised["id"], related["id"]);
            assert_eq!(revised["related_ids"], json!([id]));
            crate::app_learning::promote(&app, run).await.unwrap();
            let name = crate::app_learning::recall(&app, "com.example.reader")
                .await
                .unwrap()
                .unwrap();
            let stored = crate::workbench::skill_text(&app, &name).await.unwrap();
            assert!(stored.contains("Click the visible search entry"));
            let observed = vec![
                json!({"type":"function_call","name":"phone_observe","call_id":"current"}),
                json!({"type":"function_call_output","call_id":"current","output":[{"type":"input_text","text":json!({"success":true,"observation_id":"current-frame","foreground_app":"com.example.reader"}).to_string()}]}),
            ];
            let call = crate::worker::experience_call(
                &app,
                &observed,
                &mut crate::agent_loop::LoopState::default(),
            )
            .await
            .unwrap();
            assert_eq!(call["name"], "skill_read");
            assert_eq!(
                serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap()["name"],
                name
            );
        }
        crate::worker::steering_regressions::database_case(scenario).await;
    }
}
