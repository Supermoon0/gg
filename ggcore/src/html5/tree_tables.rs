// Included into tree.rs: the table-related insertion modes, select,
// template, frameset, the after-body tail modes, and foreign content.

impl TreeBuilder {
    // ---- InTable ------------------------------------------------------

    fn m_in_table(&mut self, t: Token) {
        match t {
            Token::Char(_) => {
                let cur = self.current();
                if self.sink.ns(cur) == Ns::Html
                    && matches!(
                        self.sink.tag(cur),
                        "table" | "tbody" | "template" | "tfoot" | "thead"
                            | "tr"
                    )
                {
                    self.pending_table_text.clear();
                    self.pending_table_text_ok = true;
                    self.original_mode = self.mode;
                    self.mode = Mode::InTableText;
                    self.by_mode(t);
                } else {
                    self.foster_in_table(t);
                }
            }
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, ref attrs, .. }
                if name == "caption" =>
            {
                self.clear_to_table_context();
                self.push_marker();
                self.insert_element(Ns::Html, "caption", attrs.clone());
                self.mode = Mode::InCaption;
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "colgroup" =>
            {
                self.clear_to_table_context();
                self.insert_element(Ns::Html, "colgroup", attrs.clone());
                self.mode = Mode::InColumnGroup;
            }
            Token::StartTag { ref name, .. } if name == "col" => {
                self.clear_to_table_context();
                self.insert_element(Ns::Html, "colgroup", Vec::new());
                self.mode = Mode::InColumnGroup;
                self.by_mode(t);
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(name.as_str(), "tbody" | "tfoot" | "thead") =>
            {
                self.clear_to_table_context();
                self.insert_element(Ns::Html, name, attrs.clone());
                self.mode = Mode::InTableBody;
            }
            Token::StartTag { ref name, .. }
                if matches!(name.as_str(), "td" | "th" | "tr") =>
            {
                self.clear_to_table_context();
                self.insert_element(Ns::Html, "tbody", Vec::new());
                self.mode = Mode::InTableBody;
                self.by_mode(t);
            }
            Token::StartTag { ref name, .. } if name == "table" => {
                if !self.in_table_scope(&["table"]) {
                    return;
                }
                self.pop_until_html("table");
                self.reset_insertion_mode();
                self.by_mode(t);
            }
            Token::EndTag { ref name } if name == "table" => {
                if !self.in_table_scope(&["table"]) {
                    return;
                }
                self.pop_until_html("table");
                self.reset_insertion_mode();
            }
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "body" | "caption" | "col" | "colgroup" | "html"
                        | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr"
                ) => {}
            Token::StartTag { ref name, .. }
                if matches!(name.as_str(), "style" | "script" | "template") =>
            {
                self.m_in_head(t)
            }
            Token::EndTag { ref name } if name == "template" => {
                self.m_in_head(t)
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "input" => {
                let hidden = attrs.iter().any(|(k, v)| {
                    k.prefix.is_none()
                        && k.local == "type"
                        && v.eq_ignore_ascii_case("hidden")
                });
                if !hidden {
                    self.foster_in_table(t);
                    return;
                }
                self.insert_element(Ns::Html, "input", attrs.clone());
                self.open.pop();
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "form" => {
                let has_template = self.open.iter().any(|&n| {
                    self.sink.is_html_element(n, "template")
                });
                if has_template || self.form.is_some() {
                    return;
                }
                let n = self.insert_element(Ns::Html, "form", attrs.clone());
                self.form = Some(n);
                self.open.pop();
            }
            Token::Eof => self.m_in_body(t),
            other => self.foster_in_table(other),
        }
    }

    /// "anything else" in a table: run it through in-body with foster
    /// parenting on, so stray content lands before the table.
    fn foster_in_table(&mut self, t: Token) {
        self.foster_parenting = true;
        self.m_in_body(t);
        self.foster_parenting = false;
    }

    fn clear_to_table_context(&mut self) {
        while {
            let n = self.current();
            !(self.sink.ns(n) == Ns::Html
                && matches!(self.sink.tag(n), "table" | "template" | "html"))
        } {
            self.open.pop();
        }
    }

    fn clear_to_table_body_context(&mut self) {
        while {
            let n = self.current();
            !(self.sink.ns(n) == Ns::Html
                && matches!(
                    self.sink.tag(n),
                    "tbody" | "tfoot" | "thead" | "template" | "html"
                ))
        } {
            self.open.pop();
        }
    }

    fn clear_to_table_row_context(&mut self) {
        while {
            let n = self.current();
            !(self.sink.ns(n) == Ns::Html
                && matches!(self.sink.tag(n), "tr" | "template" | "html"))
        } {
            self.open.pop();
        }
    }

    // ---- InTableText --------------------------------------------------

    fn m_in_table_text(&mut self, t: Token) {
        match t {
            Token::Char('\0') => {}
            Token::Char(c) => {
                self.pending_table_text.push(c);
                if !is_ws(c) {
                    self.pending_table_text_ok = false;
                }
            }
            other => {
                let text: String =
                    std::mem::take(&mut self.pending_table_text)
                        .into_iter()
                        .collect();
                let ok = self.pending_table_text_ok;
                self.pending_table_text_ok = true;
                self.mode = self.original_mode;
                if !text.is_empty() {
                    if ok {
                        self.insert_text(&text);
                    } else {
                        // non-whitespace: foster-parent it out
                        self.foster_parenting = true;
                        for c in text.chars() {
                            self.m_in_body(Token::Char(c));
                        }
                        self.foster_parenting = false;
                    }
                }
                self.by_mode(other);
            }
        }
    }

    // ---- InCaption ----------------------------------------------------

    fn m_in_caption(&mut self, t: Token) {
        match t {
            Token::EndTag { ref name } if name == "caption" => {
                if !self.in_table_scope(&["caption"]) {
                    return;
                }
                self.generate_implied_end(None);
                self.pop_until_html("caption");
                self.clear_active_to_marker();
                self.mode = Mode::InTable;
            }
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "caption" | "col" | "colgroup" | "tbody" | "td"
                        | "tfoot" | "th" | "thead" | "tr"
                ) =>
            {
                if !self.in_table_scope(&["caption"]) {
                    return;
                }
                self.generate_implied_end(None);
                self.pop_until_html("caption");
                self.clear_active_to_marker();
                self.mode = Mode::InTable;
                self.by_mode(t);
            }
            Token::EndTag { ref name } if name == "table" => {
                if !self.in_table_scope(&["caption"]) {
                    return;
                }
                self.generate_implied_end(None);
                self.pop_until_html("caption");
                self.clear_active_to_marker();
                self.mode = Mode::InTable;
                self.by_mode(t);
            }
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "body" | "col" | "colgroup" | "html" | "tbody" | "td"
                        | "tfoot" | "th" | "thead" | "tr"
                ) => {}
            other => self.m_in_body(other),
        }
    }

    // ---- InColumnGroup ------------------------------------------------

    fn m_in_column_group(&mut self, t: Token) {
        match t {
            Token::Char(c) if is_ws(c) => self.insert_char(c),
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "col" => {
                self.insert_element(Ns::Html, "col", attrs.clone());
                self.open.pop();
            }
            Token::EndTag { ref name } if name == "colgroup" => {
                if !self.sink.is_html_element(self.current(), "colgroup") {
                    return;
                }
                self.open.pop();
                self.mode = Mode::InTable;
            }
            Token::EndTag { ref name } if name == "col" => {}
            Token::StartTag { ref name, .. } if name == "template" => {
                self.m_in_head(t)
            }
            Token::EndTag { ref name } if name == "template" => {
                self.m_in_head(t)
            }
            Token::Eof => self.m_in_body(t),
            other => {
                if !self.sink.is_html_element(self.current(), "colgroup") {
                    return;
                }
                self.open.pop();
                self.mode = Mode::InTable;
                self.by_mode(other);
            }
        }
    }

    // ---- InTableBody --------------------------------------------------

    fn m_in_table_body(&mut self, t: Token) {
        match t {
            Token::StartTag { ref name, ref attrs, .. } if name == "tr" => {
                self.clear_to_table_body_context();
                self.insert_element(Ns::Html, "tr", attrs.clone());
                self.mode = Mode::InRow;
            }
            Token::StartTag { ref name, .. }
                if matches!(name.as_str(), "th" | "td") =>
            {
                self.clear_to_table_body_context();
                self.insert_element(Ns::Html, "tr", Vec::new());
                self.mode = Mode::InRow;
                self.by_mode(t);
            }
            Token::EndTag { ref name }
                if matches!(name.as_str(), "tbody" | "tfoot" | "thead") =>
            {
                if !self.in_table_scope(&[name.as_str()]) {
                    return;
                }
                self.clear_to_table_body_context();
                self.open.pop();
                self.mode = Mode::InTable;
            }
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "caption" | "col" | "colgroup" | "tbody" | "tfoot"
                        | "thead"
                ) =>
            {
                if !self.in_table_scope(&["tbody", "thead", "tfoot"]) {
                    return;
                }
                self.clear_to_table_body_context();
                self.open.pop();
                self.mode = Mode::InTable;
                self.by_mode(t);
            }
            Token::EndTag { ref name } if name == "table" => {
                if !self.in_table_scope(&["tbody", "thead", "tfoot"]) {
                    return;
                }
                self.clear_to_table_body_context();
                self.open.pop();
                self.mode = Mode::InTable;
                self.by_mode(t);
            }
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "body" | "caption" | "col" | "colgroup" | "html" | "td"
                        | "th" | "tr"
                ) => {}
            other => self.m_in_table(other),
        }
    }

    // ---- InRow --------------------------------------------------------

    fn m_in_row(&mut self, t: Token) {
        match t {
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(name.as_str(), "th" | "td") =>
            {
                self.clear_to_table_row_context();
                self.insert_element(Ns::Html, name, attrs.clone());
                self.mode = Mode::InCell;
                self.push_marker();
            }
            Token::EndTag { ref name } if name == "tr" => {
                if !self.in_table_scope(&["tr"]) {
                    return;
                }
                self.clear_to_table_row_context();
                self.open.pop();
                self.mode = Mode::InTableBody;
            }
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "caption" | "col" | "colgroup" | "tbody" | "tfoot"
                        | "thead" | "tr"
                ) =>
            {
                if !self.in_table_scope(&["tr"]) {
                    return;
                }
                self.clear_to_table_row_context();
                self.open.pop();
                self.mode = Mode::InTableBody;
                self.by_mode(t);
            }
            Token::EndTag { ref name } if name == "table" => {
                if !self.in_table_scope(&["tr"]) {
                    return;
                }
                self.clear_to_table_row_context();
                self.open.pop();
                self.mode = Mode::InTableBody;
                self.by_mode(t);
            }
            Token::EndTag { ref name }
                if matches!(name.as_str(), "tbody" | "tfoot" | "thead") =>
            {
                if !self.in_table_scope(&[name.as_str()]) {
                    return;
                }
                if !self.in_table_scope(&["tr"]) {
                    return;
                }
                self.clear_to_table_row_context();
                self.open.pop();
                self.mode = Mode::InTableBody;
                self.by_mode(t);
            }
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "body" | "caption" | "col" | "colgroup" | "html" | "td"
                        | "th"
                ) => {}
            other => self.m_in_table(other),
        }
    }

    // ---- InCell -------------------------------------------------------

    fn m_in_cell(&mut self, t: Token) {
        match t {
            Token::EndTag { ref name }
                if matches!(name.as_str(), "td" | "th") =>
            {
                if !self.in_table_scope(&[name.as_str()]) {
                    return;
                }
                self.generate_implied_end(None);
                self.pop_until_html(name);
                self.clear_active_to_marker();
                self.mode = Mode::InRow;
            }
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "caption" | "col" | "colgroup" | "tbody" | "td"
                        | "tfoot" | "th" | "thead" | "tr"
                ) =>
            {
                if !self.in_table_scope(&["td", "th"]) {
                    return;
                }
                self.close_cell();
                self.by_mode(t);
            }
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "body" | "caption" | "col" | "colgroup" | "html"
                ) => {}
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "table" | "tbody" | "tfoot" | "thead" | "tr"
                ) =>
            {
                if !self.in_table_scope(&[name.as_str()]) {
                    return;
                }
                self.close_cell();
                self.by_mode(t);
            }
            other => self.m_in_body(other),
        }
    }

    fn close_cell(&mut self) {
        self.generate_implied_end(None);
        self.pop_until_any(&["td", "th"]);
        self.clear_active_to_marker();
        self.mode = Mode::InRow;
    }

    // ---- InSelect -----------------------------------------------------

    fn m_in_select(&mut self, t: Token) {
        match t {
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "option" => {
                if self.sink.is_html_element(self.current(), "option") {
                    self.open.pop();
                }
                self.reconstruct_active();
                self.insert_element(Ns::Html, "option", attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "optgroup" =>
            {
                if self.sink.is_html_element(self.current(), "option") {
                    self.open.pop();
                }
                if self.sink.is_html_element(self.current(), "optgroup") {
                    self.open.pop();
                }
                self.reconstruct_active();
                self.insert_element(Ns::Html, "optgroup", attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "hr" => {
                if self.sink.is_html_element(self.current(), "option") {
                    self.open.pop();
                }
                if self.sink.is_html_element(self.current(), "optgroup") {
                    self.open.pop();
                }
                self.insert_element(Ns::Html, "hr", attrs.clone());
                self.open.pop();
            }
            Token::EndTag { ref name } if name == "optgroup" => {
                if self.sink.is_html_element(self.current(), "option")
                    && self.open.len() >= 2
                {
                    let below = self.open[self.open.len() - 2];
                    if self.sink.is_html_element(below, "optgroup") {
                        self.open.pop();
                    }
                }
                if self.sink.is_html_element(self.current(), "optgroup") {
                    self.open.pop();
                }
            }
            Token::EndTag { ref name } if name == "option" => {
                if self.sink.is_html_element(self.current(), "option") {
                    self.open.pop();
                }
            }
            // A <select> may now hold arbitrary content, so the old
            // "select scope" — everything but <option>/<optgroup> closes
            // the search — no longer describes it. Ordinary scope does.
            Token::EndTag { ref name } if name == "select" => {
                if !self.in_scope("select") {
                    return;
                }
                self.pop_until_html("select");
                self.reset_insertion_mode();
            }
            Token::StartTag { ref name, .. } if name == "select" => {
                if !self.in_scope("select") {
                    return;
                }
                self.pop_until_html("select");
                self.reset_insertion_mode();
            }
            // <input> is the one control that still cannot live in a
            // select; <keygen> and <textarea> now stay put.
            Token::StartTag { ref name, .. } if name == "input" => {
                if !self.in_scope("select") {
                    return;
                }
                self.pop_until_html("select");
                self.reset_insertion_mode();
                self.by_mode(t);
            }
            Token::StartTag { ref name, .. }
                if matches!(name.as_str(), "script" | "template") =>
            {
                self.m_in_head(t)
            }
            Token::EndTag { ref name } if name == "template" => {
                self.m_in_head(t)
            }
            Token::Eof => self.m_in_body(t),
            // Anything else builds normally — a <select> now keeps
            // arbitrary content, so the in-body rules apply inside it.
            other => self.m_in_body(other),
        }
    }

    // ---- InSelectInTable ----------------------------------------------

    fn m_in_select_in_table(&mut self, t: Token) {
        match t {
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr"
                        | "td" | "th"
                ) =>
            {
                self.pop_until_html("select");
                self.reset_insertion_mode();
                self.by_mode(t);
            }
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr"
                        | "td" | "th"
                ) =>
            {
                if !self.in_table_scope(&[name.as_str()]) {
                    return;
                }
                self.pop_until_html("select");
                self.reset_insertion_mode();
                self.by_mode(t);
            }
            other => self.m_in_select(other),
        }
    }

    // ---- InTemplate ---------------------------------------------------

    fn m_in_template(&mut self, t: Token) {
        match t {
            Token::Char(_) | Token::Chars(_) | Token::Comment(_)
            | Token::Doctype { .. } => {
                self.m_in_body(t)
            }
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "base" | "basefont" | "bgsound" | "link" | "meta"
                        | "noframes" | "script" | "style" | "template"
                        | "title"
                ) =>
            {
                self.m_in_head(t)
            }
            Token::EndTag { ref name } if name == "template" => {
                self.m_in_head(t)
            }
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "caption" | "colgroup" | "tbody" | "tfoot" | "thead"
                ) =>
            {
                self.template_modes.pop();
                self.template_modes.push(Mode::InTable);
                self.mode = Mode::InTable;
                self.by_mode(t);
            }
            Token::StartTag { ref name, .. } if name == "col" => {
                self.template_modes.pop();
                self.template_modes.push(Mode::InColumnGroup);
                self.mode = Mode::InColumnGroup;
                self.by_mode(t);
            }
            Token::StartTag { ref name, .. } if name == "tr" => {
                self.template_modes.pop();
                self.template_modes.push(Mode::InTableBody);
                self.mode = Mode::InTableBody;
                self.by_mode(t);
            }
            Token::StartTag { ref name, .. }
                if matches!(name.as_str(), "td" | "th") =>
            {
                self.template_modes.pop();
                self.template_modes.push(Mode::InRow);
                self.mode = Mode::InRow;
                self.by_mode(t);
            }
            Token::StartTag { .. } => {
                self.template_modes.pop();
                self.template_modes.push(Mode::InBody);
                self.mode = Mode::InBody;
                self.by_mode(t);
            }
            Token::EndTag { .. } => {}
            Token::Eof => {
                if !self.open.iter().any(|&n| {
                    self.sink.is_html_element(n, "template")
                }) {
                    self.stop();
                    return;
                }
                self.generate_implied_end_thoroughly();
                self.pop_until_html("template");
                self.clear_active_to_marker();
                self.template_modes.pop();
                self.reset_insertion_mode();
                self.by_mode(Token::Eof);
            }
        }
    }

    // ---- AfterBody and friends ----------------------------------------

    fn m_after_body(&mut self, t: Token) {
        match t {
            Token::Char(c) if is_ws(c) => self.m_in_body(Token::Char(c)),
            Token::Comment(c) => {
                let n = self.sink.push(NodeData::Comment(c));
                let root = self.open[0];
                self.sink.append(root, n);
            }
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::EndTag { ref name } if name == "html" => {
                if self.fragment {
                    return;
                }
                self.mode = Mode::AfterAfterBody;
            }
            Token::Eof => self.stop(),
            other => {
                self.mode = Mode::InBody;
                self.by_mode(other);
            }
        }
    }

    fn m_in_frameset(&mut self, t: Token) {
        match t {
            Token::Char(c) if is_ws(c) => self.insert_char(c),
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "frameset" =>
            {
                self.insert_element(Ns::Html, "frameset", attrs.clone());
            }
            Token::EndTag { ref name } if name == "frameset" => {
                if self.sink.is_html_element(self.current(), "html") {
                    return;
                }
                self.open.pop();
                if !self.fragment
                    && !self.sink.is_html_element(self.current(), "frameset")
                {
                    self.mode = Mode::AfterFrameset;
                }
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "frame" => {
                self.insert_element(Ns::Html, "frame", attrs.clone());
                self.open.pop();
            }
            Token::StartTag { ref name, .. } if name == "noframes" => {
                self.m_in_head(t)
            }
            Token::Eof => self.stop(),
            _ => {}
        }
    }

    fn m_after_frameset(&mut self, t: Token) {
        match t {
            Token::Char(c) if is_ws(c) => self.insert_char(c),
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::EndTag { ref name } if name == "html" => {
                self.mode = Mode::AfterAfterFrameset;
            }
            Token::StartTag { ref name, .. } if name == "noframes" => {
                self.m_in_head(t)
            }
            Token::Eof => self.stop(),
            _ => {}
        }
    }

    fn m_after_after_body(&mut self, t: Token) {
        match t {
            Token::Comment(c) => {
                let n = self.sink.push(NodeData::Comment(c));
                let doc = self.sink.document;
                self.sink.append(doc, n);
            }
            Token::Doctype { .. } => self.m_in_body(t),
            Token::Char(c) if is_ws(c) => self.m_in_body(Token::Char(c)),
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::Eof => self.stop(),
            other => {
                self.mode = Mode::InBody;
                self.by_mode(other);
            }
        }
    }

    fn m_after_after_frameset(&mut self, t: Token) {
        match t {
            Token::Comment(c) => {
                let n = self.sink.push(NodeData::Comment(c));
                let doc = self.sink.document;
                self.sink.append(doc, n);
            }
            Token::Doctype { .. } => self.m_in_body(t),
            Token::Char(c) if is_ws(c) => self.m_in_body(Token::Char(c)),
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::StartTag { ref name, .. } if name == "noframes" => {
                self.m_in_head(t)
            }
            Token::Eof => self.stop(),
            _ => {}
        }
    }

    // ---- foreign content ----------------------------------------------

    fn foreign(&mut self, t: Token) {
        match t {
            // The driver unpacks character runs before the modes see
            // them; handling it here too keeps the invariant local
            // rather than relying on a comment two files away.
            Token::Chars(run) => {
                for c in run.chars() {
                    self.foreign(Token::Char(c));
                }
            }
            Token::Char('\0') => self.insert_text("\u{fffd}"),
            Token::Char(c) => {
                self.insert_char(c);
                if !is_ws(c) {
                    self.frameset_ok = false;
                }
            }
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, ref attrs, self_closing } => {
                const BREAKOUT: &[&str] = &[
                    "b", "big", "blockquote", "body", "br", "center",
                    "code", "dd", "div", "dl", "dt", "em", "embed", "h1",
                    "h2", "h3", "h4", "h5", "h6", "head", "hr", "i", "img",
                    "li", "listing", "menu", "meta", "nobr", "ol", "p",
                    "pre", "ruby", "s", "small", "span", "strong",
                    "strike", "sub", "sup", "table", "tt", "u", "ul",
                    "var",
                ];
                let font_breakout = name == "font"
                    && attrs.iter().any(|(k, _)| {
                        k.prefix.is_none()
                            && matches!(
                                k.local.as_str(),
                                "color" | "face" | "size"
                            )
                    });
                if BREAKOUT.contains(&name.as_str()) || font_breakout {
                    // pop back into HTML content and reprocess
                    while {
                        let n = self.current();
                        !(self.sink.ns(n) == Ns::Html
                            || self.is_mathml_text_integration_point(n)
                            || self.is_html_integration_point(n))
                    } {
                        self.open.pop();
                    }
                    self.by_mode(t);
                    return;
                }
                let ns = self.sink.ns(
                    self.adjusted_current_node().unwrap_or(self.current()),
                );
                let mut a = attrs.clone();
                let tag = match ns {
                    Ns::MathMl => {
                        a = adjust_mathml_attrs(a);
                        name.clone()
                    }
                    Ns::Svg => {
                        a = adjust_svg_attrs(a);
                        SVG_TAG_FIXES
                            .iter()
                            .find(|(l, _)| *l == name)
                            .map(|(_, f)| (*f).to_string())
                            .unwrap_or_else(|| name.clone())
                    }
                    Ns::Html => name.clone(),
                };
                a = adjust_foreign_attrs(a);
                self.insert_element(ns, &tag, a);
                if self_closing {
                    if ns == Ns::Svg && tag == "script" {
                        self.open.pop();
                    } else {
                        self.open.pop();
                    }
                }
            }
            Token::EndTag { ref name }
                if name == "script"
                    && self.sink.ns(self.current()) == Ns::Svg
                    && self.sink.tag(self.current()) == "script" =>
            {
                self.open.pop();
            }
            Token::EndTag { ref name } => {
                // Walk up from the current node. A foreign element whose
                // name matches closes; reaching an HTML element (or the
                // root) hands the token to the insertion mode with the
                // stack untouched — `</path>` inside an <svg g> is simply
                // dropped there, while `</p>` synthesises a <p> that
                // breaks back out of the foreign subtree on its own.
                let mut i = self.open.len() - 1;
                loop {
                    if i == 0 {
                        break;
                    }
                    if self.sink.tag(self.open[i]).eq_ignore_ascii_case(name)
                    {
                        self.open.truncate(i);
                        return;
                    }
                    i -= 1;
                    if self.sink.ns(self.open[i]) == Ns::Html {
                        break;
                    }
                }
                self.by_mode(t);
            }
            Token::Eof => self.by_mode(t),
        }
    }
}
