use crate::approvals::approval_lines;
use crate::events::event_lines;
use crate::model::UiModel;
use crate::render::render;
use crate::transcript::transcript_lines;
use crate::tree::tree_lines;
use crate::wrap::wrap_lines;

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn natural_rows(model: &UiModel, cols: usize) -> usize {
    let wrap_width = cols.saturating_sub(4).max(1);
    let tree = wrap_lines(&tree_lines(&model.envs), wrap_width).len();
    let transcript = wrap_lines(
        &transcript_lines(&model.transcript, &model.expanded),
        wrap_width,
    )
    .len();
    let events = wrap_lines(&event_lines(&model.events), wrap_width).len();
    let approvals = wrap_lines(&approval_lines(&model.approvals), wrap_width).len();
    tree + transcript + events + approvals + 16
}

pub fn html(model: &UiModel, cols: usize) -> String {
    let cols = cols.max(1);
    let rows = natural_rows(model, cols);
    let body = render(model, cols, rows);
    let mut page = String::new();
    page.push_str("<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
    page.push_str("<title>tenon</title>\n<style>\n");
    page.push_str("body{background:#111;color:#ddd;font-family:monospace;margin:0;padding:1rem}\n");
    page.push_str("pre{white-space:pre;overflow-x:auto}\n");
    page.push_str("textarea{width:100%;font-family:monospace}\n");
    page.push_str("</style>\n</head>\n<body>\n");
    page.push_str("<pre>");
    page.push_str(&escape(&body));
    page.push_str("</pre>\n");
    page.push_str(prompt_form().as_str());
    for approval in &model.approvals {
        page.push_str(&approval_form(
            &approval.id,
            &approval.env,
            &approval.reason,
        ));
    }
    page.push_str(rollback_form());
    page.push_str(size_script());
    page.push_str("</body>\n</html>\n");
    page
}

fn prompt_form() -> String {
    "<form method=\"post\" action=\"/prompt\">\n<textarea name=\"text\" rows=\"4\"></textarea>\n<button type=\"submit\">Send</button>\n</form>\n".to_string()
}

fn approval_form(id: &str, env: &str, reason: &str) -> String {
    format!(
        "<form method=\"post\" action=\"/approve/{}\">\n<span>[{}] env={} reason={}</span>\n<button type=\"submit\" name=\"decision\" value=\"approve\">Approve</button>\n<button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button>\n</form>\n",
        escape(id),
        escape(id),
        escape(env),
        escape(reason)
    )
}

fn rollback_form() -> &'static str {
    "<form method=\"post\" action=\"/rollback\">\n<button type=\"submit\">Rollback</button>\n</form>\n"
}

fn size_script() -> &'static str {
    "<script>\n\
     var probe=document.createElement('span');probe.textContent='M';document.body.appendChild(probe);\n\
     var w=probe.offsetWidth||8;document.body.removeChild(probe);\n\
     var cols=Math.max(40,Math.floor(window.innerWidth/w)-2);\n\
     var current=new URLSearchParams(location.search).get('cols');\n\
     if(String(cols)!==current){location.search='?cols='+cols;}\n\
     </script>\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_special_characters() {
        let escaped = escape("<script>&\"'</script>");
        assert!(!escaped.contains('<'));
        assert!(!escaped.contains('>'));
        assert_eq!(escaped, "&lt;script&gt;&amp;&quot;&#39;&lt;/script&gt;");
    }

    #[test]
    fn page_has_forms_and_no_raw_tool_tags() {
        let mut model = UiModel::new();
        model
            .approvals
            .push(crate::model::Approval::new("a1", "root", "<danger>"));
        let page = html(&model, 100);
        assert!(page.contains("action=\"/prompt\""));
        assert!(page.contains("action=\"/approve/a1\""));
        assert!(page.contains("action=\"/rollback\""));
        assert!(page.contains("&lt;danger&gt;"));
        assert!(!page.contains("<danger>"));
    }
}
