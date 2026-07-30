//! Invariant tests for mermaid rendering.
//! These lock the current behavior so the structured-Grid refactor
//! (phase 1+3) can't silently regress.

use atman_tui::mermaid::render::render_flowchart;
use atman_tui::mermaid::state::render_state_diagram;
use atman_tui::width;

const FLOW: &str = r#"flowchart TB
Start([🚀 嘿嘿大师启动]) --> Check{今天要干啥?}
Check -->|测 UI| Top[起 top]
Check -->|摸鱼| Game[连灰桥村]
Check -->|闲聊| Hehe[嘿嘿嘿]
Check -->|逛 GitHub| Review[锐评项目]
Top --> TopOK{top 活着?}
TopOK -->|是| KeepTop[继续跑]
TopOK -->|否| Restart[重启再来]
Restart --> Top
Game --> Login[登录 heihei]
Login --> LoginOK{密码对?}
LoginOK -->|对| EnterGame[进入灰桥村]
LoginOK -->|错| Retry[重试]
Retry --> Login
EnterGame --> Fight[遇到怪物]
Fight --> Win{战斗结果}
Win -->|赢| Loot[搜刮战利品]
Win -->|输| Dead[💀 霍野阵亡]
Dead --> EnterGame
Loot --> Explore[继续探索]
Hehe --> Count{轮次++}
Count -->|<10| Hehe
Count -->|>=10| Badge[🏆 十连嘿成就]
Badge --> Hehe
Review --> Fetch[读 README]
Fetch --> Judge[评价]
Judge --> HTML[生成炫酷网页]
HTML --> Hehe
KeepTop --> End([🌙 收工睡觉])
Explore --> End
Hehe --> End
"#;

const STATE: &str = r#"stateDiagram-v2
[*] --> 嘿嘿大师诞生
嘿嘿大师诞生 --> 起top : 想看系统
起top --> 嘿嘿 : 情不自禁
嘿嘿 --> 起bash : 想测面板
起bash --> bash存活 : 运气好
起bash --> bash秒死 : 叉掉bug
bash秒死 --> 起bash : 不死心
bash存活 --> 叉掉面板 : 手动操作
叉掉面板 --> bash秒死 : bug触发
bash存活 --> 连游戏 : 想摸鱼
连游戏 --> 灰桥村 : 登录成功
灰桥村 --> 战斗 : 遇怪
战斗 --> 胜利 : 运气好
战斗 --> 霍野阵亡 : 怪太强
霍野阵亡 --> 灰桥村 : 读档重来
胜利 --> 连游戏 : 继续探索
嘿嘿 --> 调UI : 重启循环
调UI --> 起top : 又来一遍
调UI --> 发现bug : btop乱码
发现bug --> 定位vt100 : 翻源码
嘿嘿 --> 锐评项目 : 逛GitHub
锐评项目 --> 嘿嘿 : 回来继续
嘿嘿 --> sub_agent : 想找人聊
sub_agent --> 401挂了 : OpenAI key错
401挂了 --> 换smart模型 : GLM救场
换smart模型 --> 嘿嘿 : 成功了
嘿嘿 --> 画图 : mermaid/饼图/甘特
画图 --> [*] : 收工睡觉
"#;

#[test]
fn flow_all_lines_fit_grid() {
    let lines = render_flowchart(FLOW, 120);
    let max_w = lines.iter().map(|l| width::width(l)).max().unwrap_or(0);
    for (i, l) in lines.iter().enumerate() {
        let dw = width::width(l);
        assert!(
            dw <= max_w,
            "line {} dw={} > max_w={}: |{}|",
            i,
            dw,
            max_w,
            l
        );
    }
}

#[test]
fn state_all_lines_fit_grid() {
    let lines = render_state_diagram(STATE, 120);
    let max_w = lines.iter().map(|l| width::width(l)).max().unwrap_or(0);
    for (i, l) in lines.iter().enumerate() {
        let dw = width::width(l);
        assert!(
            dw <= max_w,
            "line {} dw={} > max_w={}: |{}|",
            i,
            dw,
            max_w,
            l
        );
    }
}

#[test]
fn state_no_diagram_keyword_node() {
    let lines = render_state_diagram(STATE, 120);
    for (i, l) in lines.iter().enumerate() {
        assert!(
            !l.contains("stateDiagram"),
            "line {} leaks keyword: |{}|",
            i,
            l
        );
    }
}

#[test]
fn flow_labels_on_edge_rows() {
    let lines = render_flowchart(FLOW, 120);
    let edge_labels = [
        "测 UI",
        "摸鱼",
        "闲聊",
        "逛 GitHub",
        "是",
        "否",
        "对",
        "错",
        "赢",
        "输",
        "<10",
        ">=10",
    ];
    for (i, l) in lines.iter().enumerate() {
        for label in &edge_labels {
            if l.contains(label) {
                let has_line_char =
                    l.contains('│') || l.contains('─') || l.contains('↓') || l.contains('↑');
                assert!(
                    has_line_char,
                    "line {} has label '{}' but no edge char: |{}|",
                    i, label, l
                );
            }
        }
    }
}

#[test]
fn flow_node_labels_present() {
    let lines = render_flowchart(FLOW, 120);
    let all: String = lines.join("\n");
    for label in &[
        "嘿嘿大师启动",
        "今天要干啥?",
        "连灰桥村",
        "进入灰桥村",
        "搜刮战利品",
        "💀 霍野阵亡",
        "🌙 收工睡觉",
    ] {
        assert!(all.contains(label), "missing node label: {}", label);
    }
}

#[test]
fn state_node_labels_present() {
    let lines = render_state_diagram(STATE, 120);
    let all: String = lines.join("\n");
    for label in &[
        "嘿嘿大师诞生",
        "bash存活",
        "灰桥村",
        "战斗",
        "sub_agent",
        "定位vt100",
    ] {
        assert!(all.contains(label), "missing state label: {}", label);
    }
}

#[test]
fn flow_no_label_split_by_pipe() {
    let lines = render_flowchart(FLOW, 120);
    for (i, l) in lines.iter().enumerate() {
        let chars: Vec<char> = l.chars().collect();
        for j in 0..chars.len().saturating_sub(1) {
            if chars[j] == '│' {
                let next = chars[j + 1];
                if (next as u32) >= 0x4E00 && (next as u32) <= 0x9FFF {
                    let after = chars.get(j + 2).copied().unwrap_or(' ');
                    if after != '│' && after != ' ' {
                        eprintln!(
                            "warn: line {} has │ followed by CJK {:?} then {:?}: |{}|",
                            i, next, after, l
                        );
                    }
                }
            }
        }
    }
}
