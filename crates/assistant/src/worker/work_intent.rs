//! Explicit work requirements use an action and its target in one positive clause.
struct Requirement {
    targets: &'static [&'static str],
    verbs: &'static [&'static str],
    tools: &'static [&'static str],
}

const REQUIREMENTS: &[Requirement] = &[
    Requirement {
        targets: &["skill", "技能"],
        verbs: &["创建", "新建", "写一个", "编写", "形成", "做个"],
        tools: &["skill_create"],
    },
    Requirement {
        targets: &["skill", "技能"],
        verbs: &["安装", "导入", "下载"],
        tools: &["skill_install"],
    },
    Requirement {
        targets: &["skill", "技能"],
        verbs: &["编辑", "修改", "更新", "完善"],
        tools: &["skill_edit"],
    },
    Requirement {
        targets: &["skill", "技能"],
        verbs: &["启用", "停用", "禁用"],
        tools: &["skill_set_enabled", "skill_create", "skill_install"],
    },
    Requirement {
        targets: &["知识库"],
        verbs: &[
            "保存", "创建", "新建", "整理", "更新", "编辑", "写入", "修改", "形成", "记入", "收录",
        ],
        tools: &["knowledge_write"],
    },
    Requirement {
        targets: &["知识库"],
        verbs: &["关联", "链接", "解除关联"],
        tools: &["knowledge_link", "knowledge_write"],
    },
    Requirement {
        targets: &[
            "文件", "脚本", "报告", "文档", "工作", "python", "node", "bun", "代码", "网页",
            "资源", "pdf", "docx", "表格",
        ],
        verbs: &[
            "执行", "运行", "生成", "创建", "写入", "保存", "下载", "处理", "编写",
        ],
        tools: &[
            "workspace_write",
            "workspace_run",
            "workspace_fetch",
            "computer_run",
            "computer_type",
            "device_transfer",
        ],
    },
    Requirement {
        // A GUI menu item is also a 项目; that word alone is not an external lookup.
        targets: &["github", "官网", "网站", "网页", "资料", "skill", "仓库"],
        verbs: &[
            "找找",
            "查找",
            "检索",
            "搜索",
            "搜一下",
            "查一下",
            "调研",
            "去官网",
            "官网看看",
        ],
        tools: &[
            "computer_click",
            "computer_type",
            "computer_key",
            "phone_tap_element",
            "phone_input",
            "phone_key",
            "workspace_fetch",
            "workspace_run",
        ],
    },
    Requirement {
        targets: &["记住"],
        verbs: &["记住"],
        tools: &["memory_save"],
    },
];

pub(super) fn positive_clauses(request: &str) -> impl Iterator<Item = &str> {
    request
        .split(['。', '，', ',', '；', ';', '！', '!', '？', '?', '\n'])
        .flat_map(|clause| clause.split('但'))
        .map(str::trim)
        .filter(|clause| {
            ![
                "不要",
                "不使用",
                "别用",
                "无需",
                "禁止",
                "不得",
                "不用",
                "不必",
                "不执行",
                "不运行",
                "不生成",
                "不创建",
                "不写入",
                "不保存",
                "不下载",
                "不处理",
                "不操作",
                "不编写",
                "不浏览",
                "不联网",
                "不搜索",
                "不安装",
                "不导入",
                "不编辑",
                "不修改",
                "不更新",
                "不完善",
                "不形成",
                "不启用",
                "不停用",
                "不禁用",
                "不记住",
                "do not",
                "don't",
                "without",
                "no shell",
                "no terminal",
            ]
            .iter()
            .any(|negative| clause.contains(negative))
        })
}

pub(super) fn harness_resource_request(request: &str) -> bool {
    [
        "skill",
        "技能",
        "知识库",
        "工作目录",
        "工作文件",
        "脚本",
        "代码",
        "python",
        "node",
        "bun",
    ]
    .iter()
    .any(|word| request.contains(word))
}

pub(super) fn work_subject(request: &str) -> bool {
    harness_resource_request(request)
        || ["文档", "pdf", "docx", "表格"]
            .iter()
            .any(|word| request.contains(word))
        || (request.contains("报告") && required_tools(request).contains(&"workspace_write"))
}

pub(super) fn affirmed_action(request: &str, verbs: &[&str]) -> bool {
    positive_clauses(request).any(|clause| {
        verbs.iter().any(|verb| {
            clause.match_indices(verb).any(|(index, _)| {
                let prefix = clause[..index].trim_end();
                ![
                    "不", "别", "勿", "不要", "不准", "不能", "不可", "禁止", "避免", "无需",
                    "不必", "未", "not", "never",
                ]
                .iter()
                .any(|negative| prefix.ends_with(negative))
            })
        })
    })
}

pub(super) fn inspection_request(request: &str) -> bool {
    affirmed_action(
        request,
        &[
            "观察",
            "查看",
            "看看",
            "看下",
            "看一下",
            "检查",
            "读取",
            "observe",
            "inspect",
            "look at",
            "view",
            "check",
        ],
    )
}

fn matches(requirement: &Requirement, clause: &str) -> bool {
    requirement
        .targets
        .iter()
        .any(|target| clause.contains(target))
        && requirement.verbs.iter().any(|verb| clause.contains(verb))
}

pub(super) fn required_tools(request: &str) -> Vec<&'static str> {
    let clauses: Vec<_> = positive_clauses(request).collect();
    REQUIREMENTS
        .iter()
        .find(|requirement| clauses.iter().any(|clause| matches(requirement, clause)))
        .map(|requirement| requirement.tools.to_vec())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_reports_do_not_require_creating_a_work_artifact() {
        assert!(!work_subject("观察手机当前画面，报告前台状态"));
        assert!(work_subject("在工作目录生成报告文件"));
        assert!(work_subject("读取skill列表"));
        assert!(work_subject("打开表格文档"));
        assert!(harness_resource_request("只读取skill列表"));
        assert!(!harness_resource_request("打开表格文档"));
    }

    #[test]
    fn read_only_inspection_never_turns_excluded_input_into_an_action() {
        assert!(inspection_request("只观察手机当前画面，不点击也不输入"));
        assert!(!affirmed_action(
            "只观察手机当前画面，不点击也不输入",
            &["点击", "输入"]
        ));
        assert!(affirmed_action("观察手机画面，然后点击输入框", &["点击"]));
        assert!(!inspection_request("不要观察手机，只解释原理"));
    }

    #[test]
    fn explicit_positive_work_keeps_its_required_evidence() {
        for (request, tool) in [
            ("创建一个skill并启用", "skill_create"),
            ("安装skill但不要运行它的脚本", "skill_install"),
            ("更新skill", "skill_edit"),
            ("禁用skill", "skill_set_enabled"),
            ("将说明保存到知识库", "knowledge_write"),
            ("生成报告文件", "workspace_write"),
            ("搜索github资料", "workspace_fetch"),
            ("记住我的语言偏好", "memory_save"),
            ("安装skill；然后创建skill", "skill_create"),
        ] {
            assert!(required_tools(request).contains(&tool), "{request}");
        }
    }

    #[test]
    fn exclusions_and_unrelated_targets_never_create_work_requirements() {
        for request in [
            "读取skill，不安装下载",
            "不要修改知识库",
            "不记住这条消息",
            "读取skill，安装微信",
            "定位Find项目并点击打开查找界面",
            "只查看报告，不生成文件",
            "不要搜索github资料",
            "查看屏幕",
            "",
        ] {
            assert!(required_tools(request).is_empty(), "{request}");
        }
    }
}
