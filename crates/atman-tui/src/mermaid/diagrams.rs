pub struct ErEntity {
    pub name: String,
}

pub struct ErRelationship {
    pub from: String,
    pub to: String,
    pub label: String,
}

pub struct ErDiagram {
    pub entities: Vec<ErEntity>,
    pub relationships: Vec<ErRelationship>,
}

pub fn parse_er(source: &str) -> ErDiagram {
    let mut er = ErDiagram {
        entities: Vec::new(),
        relationships: Vec::new(),
    };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "erDiagram" {
            continue;
        }
        if let Some(pos) = line.find("||--") {
            let from = line[..pos].trim().to_string();
            let rest = &line[pos + 4..];
            if let Some(colon) = rest.find(':') {
                let to_part = rest[..colon].trim();
                let parts: Vec<&str> = to_part.split_whitespace().collect();
                let to = parts.last().unwrap_or(&"").to_string();
                let label = rest[colon + 1..].trim().trim_matches('"').to_string();
                if !er.entities.iter().any(|e| e.name == from) {
                    er.entities.push(ErEntity { name: from.clone() });
                }
                if !er.entities.iter().any(|e| e.name == to) {
                    er.entities.push(ErEntity { name: to.clone() });
                }
                er.relationships.push(ErRelationship { from, to, label });
            }
        } else if let Some(name) = line.strip_suffix(" {") {
            let name = name.trim().to_string();
            if !er.entities.iter().any(|e| e.name == name) {
                er.entities.push(ErEntity { name });
            }
        }
    }
    er
}

pub fn render_er(er: &ErDiagram) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, entity) in er.entities.iter().enumerate() {
        let name_w = crate::width::width(entity.name.as_str());
        lines.push(format!("  ┌─{}─┐", "─".repeat(name_w)));
        lines.push(format!("  │ {} │", entity.name));
        lines.push(format!("  └─{}─┘", "─".repeat(name_w)));
        if i + 1 < er.entities.len() {
            lines.push("      │".to_string());
            lines.push("      ▼".to_string());
        }
    }
    if !er.relationships.is_empty() {
        lines.push(String::new());
        lines.push("  Relationships:".to_string());
        for rel in &er.relationships {
            lines.push(format!(
                "    {} ──{}── {} {}",
                rel.from, rel.label, rel.to, ""
            ));
        }
    }
    lines
}

pub struct ClassMember {
    pub visibility: String,
    pub name: String,
}

pub struct ClassDef {
    pub name: String,
    pub members: Vec<ClassMember>,
}

pub struct ClassRel {
    pub from: String,
    pub to: String,
    pub label: String,
}

pub struct ClassDiagram {
    pub classes: Vec<ClassDef>,
    pub relationships: Vec<ClassRel>,
}

pub fn parse_class(source: &str) -> ClassDiagram {
    let mut cd = ClassDiagram {
        classes: Vec::new(),
        relationships: Vec::new(),
    };
    let mut current_class: Option<ClassDef> = None;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "classDiagram" {
            continue;
        }
        if line.starts_with("class ") {
            if let Some(c) = current_class.take() {
                cd.classes.push(c);
            }
            let name = line
                .strip_prefix("class ")
                .unwrap_or("")
                .trim_end_matches('{')
                .trim()
                .to_string();
            current_class = Some(ClassDef {
                name,
                members: Vec::new(),
            });
            continue;
        }
        if line == "}" {
            if let Some(c) = current_class.take() {
                cd.classes.push(c);
            }
            continue;
        }
        if let Some(cls) = current_class.as_mut() {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 {
                cls.members.push(ClassMember {
                    visibility: parts[0].to_string(),
                    name: parts[1].to_string(),
                });
            } else {
                cls.members.push(ClassMember {
                    visibility: "".to_string(),
                    name: parts[0].to_string(),
                });
            }
            continue;
        }
        if let Some(pos) = line.find("--|>") {
            let from = line[..pos].trim().to_string();
            let to = line[pos + 4..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            cd.relationships.push(ClassRel {
                from,
                to,
                label: "extends".to_string(),
            });
        } else if let Some(pos) = line.find("-->") {
            let from = line[..pos].trim().to_string();
            let rest = &line[pos + 3..];
            let parts: Vec<&str> = rest.splitn(2, ':').collect();
            let to = parts[0].trim().to_string();
            let label = if parts.len() > 1 {
                parts[1].trim().to_string()
            } else {
                String::new()
            };
            cd.relationships.push(ClassRel { from, to, label });
        }
    }
    if let Some(c) = current_class {
        cd.classes.push(c);
    }
    cd
}

pub fn render_class(cd: &ClassDiagram) -> Vec<String> {
    let mut lines = Vec::new();
    for cls in &cd.classes {
        let max_w = crate::width::width(cls.name.as_str()).max(
            cls.members
                .iter()
                .map(|m| {
                    crate::width::width(m.name.as_str())
                        + crate::width::width(m.visibility.as_str())
                        + 1
                })
                .max()
                .unwrap_or(0),
        );
        let w = max_w + 2;
        lines.push(format!("  ╭─{}─╮", "─".repeat(w)));
        lines.push(format!("  │ {} │", cls.name));
        lines.push(format!("  ├─{}─┤", "─".repeat(w)));
        if cls.members.is_empty() {
            lines.push(format!("  │ {} │", " ".repeat(w)));
        } else {
            for m in &cls.members {
                let entry = if m.visibility.is_empty() {
                    m.name.clone()
                } else {
                    format!("{} {}", m.visibility, m.name)
                };
                let entry_w = crate::width::width(entry.as_str());
                let pad = w.saturating_sub(entry_w);
                lines.push(format!("  │ {}{} │", entry, " ".repeat(pad)));
            }
        }
        lines.push(format!("  ╰─{}─╯", "─".repeat(w)));
        lines.push(String::new());
    }
    if !cd.relationships.is_empty() {
        lines.push("  Relationships:".to_string());
        for rel in &cd.relationships {
            if rel.label.is_empty() {
                lines.push(format!("    {} ──→ {}", rel.from, rel.to));
            } else {
                lines.push(format!("    {} ──{}──→ {}", rel.from, rel.label, rel.to));
            }
        }
    }
    lines
}

pub struct MindmapNode {
    pub label: String,
    pub children: Vec<MindmapNode>,
}

pub struct Mindmap {
    pub root: Option<MindmapNode>,
}

pub fn parse_mindmap(source: &str) -> Mindmap {
    let mut root: Option<MindmapNode> = None;
    let mut stack: Vec<(usize, MindmapNode)> = Vec::new();

    for line in source.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() || line.trim().starts_with("%%") || line.trim() == "mindmap" {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let label = line
            .trim()
            .trim_start_matches("((")
            .trim_end_matches("))")
            .to_string();
        let node = MindmapNode {
            label,
            children: Vec::new(),
        };

        while stack.last().is_some_and(|(ind, _)| *ind >= indent) {
            let (_, popped) = stack.pop().unwrap();
            if let Some(parent) = stack.last_mut() {
                parent.1.children.push(popped);
            } else if root.is_none() {
                root = Some(popped);
            } else {
                stack.push((0, popped));
                break;
            }
        }
        stack.push((indent, node));
    }

    while let Some((_, popped)) = stack.pop() {
        if let Some(parent) = stack.last_mut() {
            parent.1.children.push(popped);
        } else if root.is_none() {
            root = Some(popped);
        }
    }

    Mindmap { root }
}

pub fn render_mindmap(mm: &Mindmap) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(root) = &mm.root {
        render_mindmap_node(root, 0, &mut lines);
    }
    lines
}

fn render_mindmap_node(node: &MindmapNode, depth: usize, lines: &mut Vec<String>) {
    let prefix = if depth == 0 {
        "  ".to_string()
    } else {
        format!("  {}├─ ", "│ ".repeat(depth - 1))
    };
    lines.push(format!("{prefix}{}", node.label));
    for child in &node.children {
        render_mindmap_node(child, depth + 1, lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_er_basic() {
        let er = parse_er("erDiagram\nA ||--|| B : has");
        assert_eq!(er.entities.len(), 2);
        assert_eq!(er.relationships.len(), 1);
        assert_eq!(er.relationships[0].label, "has");
    }

    #[test]
    fn parse_class_basic() {
        let cd = parse_class("classDiagram\nclass Foo {\n+bar()\n}");
        assert_eq!(cd.classes.len(), 1);
        assert_eq!(cd.classes[0].members.len(), 1);
    }

    #[test]
    fn parse_mindmap_basic() {
        let mm = parse_mindmap("mindmap\n  root\n    child1\n    child2");
        assert!(mm.root.is_some());
        assert_eq!(mm.root.as_ref().unwrap().label, "root");
        assert_eq!(mm.root.as_ref().unwrap().children.len(), 2);
    }
}
