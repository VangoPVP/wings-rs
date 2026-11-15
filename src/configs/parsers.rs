use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct Replacement {
    pub key: String,
    pub if_value: Option<String>,
    pub value: Value,
}

pub fn apply(parser: &str, content: &str, reps: &[Replacement]) -> Result<String> {
    match parser {
        "properties" => Ok(properties(content, reps)),
        "file" => Ok(plain_file(content, reps)),
        "json" => json(content, reps),
        "yaml" | "yml" => yaml(content, reps),
        "ini" => Ok(ini(content, reps)),
        "xml" => xml(content, reps),
        "toml" => toml(content, reps),
        other => bail!("unsupported config parser '{other}'"),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn if_matches(current: Option<&str>, rep: &Replacement) -> bool {
    match &rep.if_value {
        None => true,
        Some(want) => current == Some(want.as_str()),
    }
}

fn join_lines(lines: &[String], original: &str) -> String {
    let mut out = lines.join("\n");
    if original.is_empty() || original.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn properties_kv(line: &str) -> Option<(String, String)> {
    let t = line.trim_start();
    if t.is_empty() || t.starts_with('#') || t.starts_with('!') {
        return None;
    }
    let mut key = String::new();
    let mut chars = t.char_indices().peekable();
    let mut rest_at = t.len();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                if let Some((_, n)) = chars.next() {
                    key.push(n);
                }
            }
            '=' | ':' => {
                rest_at = i + 1;
                break;
            }
            c if c.is_whitespace() => {
                let after = t[i..].trim_start();
                rest_at = t.len() - after.len();
                if after.starts_with('=') || after.starts_with(':') {
                    rest_at += 1;
                }
                break;
            }
            c => key.push(c),
        }
    }
    Some((key, t[rest_at..].trim_start().to_string()))
}

fn properties(content: &str, reps: &[Replacement]) -> String {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    for rep in reps {
        let mut found = false;
        let mut i = 0;
        while i < lines.len() {
            if let Some((k, v)) = properties_kv(&lines[i])
                && k == rep.key
            {
                found = true;
                if if_matches(Some(v.trim_end()), rep) {
                    let continues = |l: &str| l.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1;
                    let mut end = i;
                    while continues(&lines[end]) && end + 1 < lines.len() {
                        end += 1;
                    }
                    lines.splice(i..=end, [format!("{}={}", rep.key, text(&rep.value))]);
                }
            }
            i += 1;
        }
        if !found && rep.if_value.is_none() {
            lines.push(format!("{}={}", rep.key, text(&rep.value)));
        }
    }
    join_lines(&lines, content)
}

fn plain_file(content: &str, reps: &[Replacement]) -> String {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    for rep in reps {
        if rep.key.is_empty() {
            continue;
        }
        for line in lines.iter_mut() {
            if line.starts_with(&rep.key) && if_matches(Some(line.as_str()), rep) {
                *line = text(&rep.value);
            }
        }
    }
    join_lines(&lines, content)
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::Object(_) | Value::Array(_) => None,
        other => Some(text(other)),
    }
}

fn set_path(cur: &mut Value, segs: &[&str], rep: &Replacement) {
    let Some((seg, rest)) = segs.split_first() else {
        if if_matches(scalar_text(cur).as_deref(), rep) {
            *cur = rep.value.clone();
        }
        return;
    };
    if *seg == "*" {
        match cur {
            Value::Object(m) => m.values_mut().for_each(|v| set_path(v, rest, rep)),
            Value::Array(a) => a.iter_mut().for_each(|v| set_path(v, rest, rep)),
            _ => {}
        }
        return;
    }
    if let Value::Array(a) = cur {
        if let Ok(i) = seg.parse::<usize>()
            && let Some(v) = a.get_mut(i)
        {
            set_path(v, rest, rep);
        }
        return;
    }
    if cur.is_null() {
        if rep.if_value.is_some() {
            return;
        }
        *cur = Value::Object(Map::new());
    }
    let Value::Object(m) = cur else { return };
    match m.get_mut(*seg) {
        Some(child) => set_path(child, rest, rep),
        None if rep.if_value.is_none() => {
            let mut child = Value::Null;
            set_path(&mut child, rest, rep);
            m.insert(seg.to_string(), child);
        }
        None => {}
    }
}

fn apply_paths(doc: &mut Value, reps: &[Replacement]) {
    for rep in reps {
        let segs: Vec<&str> = rep.key.split('.').filter(|s| !s.is_empty()).collect();
        if !segs.is_empty() {
            set_path(doc, &segs, rep);
        }
    }
}

fn json(content: &str, reps: &[Replacement]) -> Result<String> {
    let mut doc: Value = if content.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(content).context("invalid JSON")?
    };
    apply_paths(&mut doc, reps);
    let mut out = serde_json::to_string_pretty(&doc)?;
    out.push('\n');
    Ok(out)
}

fn yaml(content: &str, reps: &[Replacement]) -> Result<String> {
    let mut doc: Value = if content.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_saphyr::from_str(content).map_err(|e| anyhow::anyhow!("invalid YAML: {e}"))?
    };
    apply_paths(&mut doc, reps);
    serde_saphyr::to_string(&doc).map_err(|e| anyhow::anyhow!("serialising YAML: {e}"))
}

fn ini_section_name(line: &str) -> Option<&str> {
    let t = line.trim();
    t.strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

fn ini_kv(line: &str) -> Option<(&str, &str)> {
    let t = line.trim_start();
    if t.is_empty() || t.starts_with(';') || t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    let (k, v) = t.split_once('=').or_else(|| t.split_once(':'))?;
    Some((k.trim(), v.trim()))
}

fn ini(content: &str, reps: &[Replacement]) -> String {
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    for rep in reps {
        let (section, key) = match rep.key.split_once('.') {
            Some((s, k)) => (s.to_string(), k.to_string()),
            None => (String::new(), rep.key.clone()),
        };
        let value = text(&rep.value);
        let mut cur = String::new();
        let mut found = false;
        let mut insert_at: Option<usize> = if section.is_empty() { Some(0) } else { None };
        for (i, line) in lines.iter_mut().enumerate() {
            if let Some(name) = ini_section_name(line) {
                cur = name.to_string();
                if cur == section {
                    insert_at = Some(i + 1);
                }
                continue;
            }
            if cur != section {
                continue;
            }
            if !line.trim().is_empty() {
                insert_at = Some(i + 1);
            }
            if let Some((k, v)) = ini_kv(line)
                && k == key
            {
                found = true;
                if if_matches(Some(v), rep) {
                    let sep = line.find('=').or_else(|| line.find(':')).unwrap_or(line.len());
                    let (head, tail) = line.split_at(sep + 1);
                    let spacing = if tail.starts_with(' ') { " " } else { "" };
                    *line = format!("{head}{spacing}{value}");
                }
            }
        }
        if found || rep.if_value.is_some() {
            continue;
        }
        let new_line = format!("{key}={value}");
        match insert_at {
            Some(i) => lines.insert(i, new_line),
            None => {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(format!("[{section}]"));
                lines.push(new_line);
            }
        }
    }
    join_lines(&lines, content)
}

fn xml(content: &str, reps: &[Replacement]) -> Result<String> {
    use xmltree::{Element, EmitterConfig, XMLNode};
    let mut root = if content.trim().is_empty() {
        None
    } else {
        Some(Element::parse(content.as_bytes()).context("invalid XML")?)
    };

    fn set(el: &mut Element, segs: &[&str], rep: &Replacement) {
        let Some((seg, rest)) = segs.split_first() else {
            let current = el.get_text().map(|t| t.into_owned());
            if if_matches(current.as_deref(), rep) {
                el.children.retain(|c| !matches!(c, XMLNode::Text(_) | XMLNode::CData(_)));
                el.children.insert(0, XMLNode::Text(text(&rep.value)));
            }
            return;
        };
        if let Some(attr) = seg.strip_prefix('@') {
            if rest.is_empty() {
                let current = el.attributes.get(attr).cloned();
                if if_matches(current.as_deref(), rep)
                    && (current.is_some() || rep.if_value.is_none())
                {
                    el.attributes.insert(attr.to_string(), text(&rep.value));
                }
            }
            return;
        }
        if *seg == "*" {
            for c in el.children.iter_mut() {
                if let XMLNode::Element(child) = c {
                    set(child, rest, rep);
                }
            }
            return;
        }
        let mut matched = false;
        for c in el.children.iter_mut() {
            if let XMLNode::Element(child) = c
                && child.name == *seg
            {
                matched = true;
                set(child, rest, rep);
            }
        }
        if !matched && rep.if_value.is_none() {
            let mut child = Element::new(seg);
            set(&mut child, rest, rep);
            el.children.push(XMLNode::Element(child));
        }
    }

    for rep in reps {
        let mut segs: Vec<&str> = rep.key.split('.').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            continue;
        }
        let root_el = root.get_or_insert_with(|| Element::new(segs[0]));
        if segs.len() > 1 && segs[0] == root_el.name {
            segs.remove(0);
        }
        set(root_el, &segs, rep);
    }

    let Some(root) = root else {
        return Ok(content.to_string());
    };
    let mut out = Vec::new();
    root.write_with_config(
        &mut out,
        EmitterConfig::new()
            .perform_indent(true)
            .write_document_declaration(true),
    )?;
    let mut s = String::from_utf8(out)?;
    s.push('\n');
    Ok(s)
}

fn toml_value(v: &Value) -> Option<toml_edit::Value> {
    Some(match v {
        Value::String(s) => s.as_str().into(),
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64()?.into(),
        },
        Value::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(toml_value(x)?);
            }
            toml_edit::Value::Array(arr)
        }
        Value::Null | Value::Object(_) => return None,
    })
}

fn toml_item_text(item: &toml_edit::Item) -> Option<String> {
    let v = item.as_value()?;
    Some(match v {
        toml_edit::Value::String(s) => s.value().clone(),
        other => other.to_string().trim().to_string(),
    })
}

fn toml(content: &str, reps: &[Replacement]) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = content.parse().context("invalid TOML")?;

    fn set(item: &mut toml_edit::Item, segs: &[&str], rep: &Replacement, new: &toml_edit::Value) {
        let Some((seg, rest)) = segs.split_first() else {
            if if_matches(toml_item_text(item).as_deref(), rep) {
                let decor = item.as_value().map(|v| v.decor().clone());
                let mut v = new.clone();
                if let Some(d) = decor {
                    *v.decor_mut() = d;
                }
                *item = toml_edit::Item::Value(v);
            }
            return;
        };
        if *seg == "*" {
            if let Some(t) = item.as_table_like_mut() {
                for (_, child) in t.iter_mut() {
                    set(child, rest, rep, new);
                }
            } else if let Some(arr) = item.as_array_of_tables_mut() {
                for t in arr.iter_mut() {
                    for (_, child) in t.iter_mut() {
                        let _ = child;
                    }
                    let mut wrapped = toml_edit::Item::Table(t.clone());
                    set(&mut wrapped, rest, rep, new);
                    if let toml_edit::Item::Table(nt) = wrapped {
                        *t = nt;
                    }
                }
            }
            return;
        }
        if let Some(arr) = item.as_array_of_tables_mut() {
            if let Ok(i) = seg.parse::<usize>()
                && let Some(t) = arr.get_mut(i)
            {
                let mut wrapped = toml_edit::Item::Table(t.clone());
                set(&mut wrapped, rest, rep, new);
                if let toml_edit::Item::Table(nt) = wrapped {
                    *t = nt;
                }
            }
            return;
        }
        let Some(t) = item.as_table_like_mut() else { return };
        match t.get_mut(seg) {
            Some(child) => set(child, rest, rep, new),
            None if rep.if_value.is_none() => {
                let mut child = if rest.is_empty() {
                    toml_edit::Item::None
                } else {
                    toml_edit::Item::Table(toml_edit::Table::new())
                };
                set(&mut child, rest, rep, new);
                t.insert(seg, child);
            }
            None => {}
        }
    }

    for rep in reps {
        let segs: Vec<&str> = rep.key.split('.').filter(|s| !s.is_empty()).collect();
        let Some(new) = toml_value(&rep.value) else {
            continue;
        };
        if segs.is_empty() {
            continue;
        }
        set(doc.as_item_mut(), &segs, rep, &new);
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r(key: &str, v: Value) -> Replacement {
        Replacement {
            key: key.into(),
            if_value: None,
            value: v,
        }
    }
    fn rif(key: &str, ifv: &str, v: Value) -> Replacement {
        Replacement {
            key: key.into(),
            if_value: Some(ifv.into()),
            value: v,
        }
    }

    #[test]
    fn properties_minecraft() {
        let src = "#Minecraft server properties\n#Fri Nov 14\nserver-port=25565\nmotd=A Minecraft Server\nonline-mode=true\n";
        let out = apply(
            "properties",
            src,
            &[
                r("server-port", json!(25599)),
                r("server-ip", json!("0.0.0.0")),
                r("motd", json!("hello world")),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            "#Minecraft server properties\n#Fri Nov 14\nserver-port=25599\nmotd=hello world\nonline-mode=true\nserver-ip=0.0.0.0\n"
        );
    }

    #[test]
    fn properties_separators_and_if_value() {
        let src = "a = 1\nb: 2\nc 3\n! comment = x\n";
        let out = apply(
            "properties",
            src,
            &[r("a", json!("x")), r("b", json!(true)), r("c", json!(9)), rif("d", "1", json!(5))],
        )
        .unwrap();
        assert_eq!(out, "a=x\nb=true\nc=9\n! comment = x\n");
        let out = apply("properties", "a=1\n", &[rif("a", "2", json!(3))]).unwrap();
        assert_eq!(out, "a=1\n");
        let out = apply("properties", "a=1\n", &[rif("a", "1", json!(3))]).unwrap();
        assert_eq!(out, "a=3\n");
    }

    #[test]
    fn properties_empty_and_continuation() {
        assert_eq!(apply("properties", "", &[r("k", json!("v"))]).unwrap(), "k=v\n");
        let src = "list=a,\\\n  b,\\\n  c\nother=1";
        let out = apply("properties", src, &[r("list", json!("x"))]).unwrap();
        assert_eq!(out, "list=x\nother=1");
    }

    #[test]
    fn plain_file_prefix() {
        let src = "port 1\nname a\nport 2\n";
        let out = apply("file", src, &[r("port", json!("port 25599"))]).unwrap();
        assert_eq!(out, "port 25599\nname a\nport 25599\n");
        assert_eq!(apply("file", "x\n", &[r("nomatch", json!("y"))]).unwrap(), "x\n");
    }

    #[test]
    fn json_paths_types_and_order() {
        let src = r#"{"z": 1, "server": {"port": 1, "host": "a"}, "list": [{"h": 1}, {"h": 2}]}"#;
        let out = apply(
            "json",
            src,
            &[
                r("server.port", json!(25599)),
                r("list.*.h", json!("x")),
                r("list.0.extra", json!(true)),
                r("new.deep.key", json!(false)),
                rif("server.host", "nope", json!("b")),
            ],
        )
        .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["server"]["port"], json!(25599));
        assert_eq!(v["server"]["host"], json!("a"));
        assert_eq!(v["list"][0]["h"], json!("x"));
        assert_eq!(v["list"][1]["h"], json!("x"));
        assert_eq!(v["list"][0]["extra"], json!(true));
        assert_eq!(v["new"]["deep"]["key"], json!(false));
        assert!(out.find("\"z\"").unwrap() < out.find("\"server\"").unwrap(), "order kept");
        assert!(apply("json", "{not json", &[]).is_err());
        let empty = apply("json", "", &[r("a.b", json!(1))]).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&empty).unwrap(), json!({"a": {"b": 1}}));
    }

    #[test]
    fn yaml_bungeecord() {
        let src = "listeners:\n- host: 0.0.0.0:25577\n  motd: hi\n  max_players: 1\n- host: 1.2.3.4:1\n  motd: two\nonline_mode: true\n";
        let out = apply(
            "yaml",
            src,
            &[
                r("listeners.*.host", json!("0.0.0.0:25600")),
                r("listeners.0.max_players", json!(50)),
                r("ip_forward", json!(true)),
            ],
        )
        .unwrap();
        let v: Value = serde_saphyr::from_str(&out).unwrap();
        assert_eq!(v["listeners"][0]["host"], json!("0.0.0.0:25600"));
        assert_eq!(v["listeners"][1]["host"], json!("0.0.0.0:25600"));
        assert_eq!(v["listeners"][0]["max_players"], json!(50));
        assert_eq!(v["online_mode"], json!(true));
        assert_eq!(v["ip_forward"], json!(true));
        assert!(apply("yaml", "a: [unclosed", &[]).is_err());
        let fresh = apply("yaml", "", &[r("server.port", json!(1))]).unwrap();
        let v: Value = serde_saphyr::from_str(&fresh).unwrap();
        assert_eq!(v["server"]["port"], json!(1));
    }

    #[test]
    fn ini_sections() {
        let src = "; top comment\nglobal=1\n\n[server]\nport = 1\n# c\nname=a\n\n[other]\nx=1\n";
        let out = apply(
            "ini",
            src,
            &[
                r("server.port", json!(25599)),
                r("server.added", json!("y")),
                r("global", json!(2)),
                r("fresh.k", json!("v")),
                rif("other.x", "9", json!(5)),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            "; top comment\nglobal=2\n\n[server]\nport = 25599\n# c\nname=a\nadded=y\n\n[other]\nx=1\n\n[fresh]\nk=v\n"
        );
        assert_eq!(apply("ini", "", &[r("a", json!(1))]).unwrap(), "a=1\n");
    }

    #[test]
    fn xml_paths_and_attributes() {
        let src = r#"<?xml version="1.0" encoding="UTF-8"?><config><server port="1"><name>a</name></server><item>1</item><item>2</item></config>"#;
        let out = apply(
            "xml",
            src,
            &[
                r("config.server.name", json!("b")),
                r("server.@port", json!(25599)),
                r("item", json!("x")),
                r("server.motd", json!("hi")),
            ],
        )
        .unwrap();
        assert!(out.starts_with("<?xml"));
        let root = xmltree::Element::parse(out.as_bytes()).unwrap();
        let server = root.get_child("server").unwrap();
        assert_eq!(server.attributes.get("port").unwrap(), "25599");
        assert_eq!(server.get_child("name").unwrap().get_text().unwrap(), "b");
        assert_eq!(server.get_child("motd").unwrap().get_text().unwrap(), "hi");
        let items: Vec<_> = root
            .children
            .iter()
            .filter_map(|c| c.as_element())
            .filter(|e| e.name == "item")
            .map(|e| e.get_text().unwrap().into_owned())
            .collect();
        assert_eq!(items, vec!["x", "x"]);
        assert!(apply("xml", "<a><b></a>", &[]).is_err());
    }

    #[test]
    fn toml_preserves_comments() {
        let src = "# top\n[server]\nport = 1 # the port\nname = \"a\"\n\n[[players]]\nname = \"x\"\n[[players]]\nname = \"y\"\n";
        let out = apply(
            "toml",
            src,
            &[
                r("server.port", json!(25599)),
                r("server.new", json!(true)),
                r("players.*.name", json!("z")),
                r("extra.key", json!("v")),
            ],
        )
        .unwrap();
        assert!(out.contains("# top"));
        assert!(out.contains("port = 25599 # the port"), "{out}");
        let doc: toml_edit::DocumentMut = out.parse().unwrap();
        assert_eq!(doc["server"]["new"].as_bool(), Some(true));
        assert_eq!(doc["players"][0]["name"].as_str(), Some("z"));
        assert_eq!(doc["players"][1]["name"].as_str(), Some("z"));
        assert_eq!(doc["extra"]["key"].as_str(), Some("v"));
        assert!(apply("toml", "a = = 1", &[]).is_err());
    }

    #[test]
    fn unknown_parser_errors() {
        assert!(apply("bogus", "", &[]).is_err());
    }
}
