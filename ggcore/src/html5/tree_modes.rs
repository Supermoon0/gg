// Included into tree.rs: the insertion modes proper. Split out only
// so neither file becomes unreadable; this is one continuous piece of
// the tree construction stage.

/// SVG tag names that are not all-lowercase in the DOM.
const SVG_TAG_FIXES: &[(&str, &str)] = &[
    ("altglyph", "altGlyph"),
    ("altglyphdef", "altGlyphDef"),
    ("altglyphitem", "altGlyphItem"),
    ("animatecolor", "animateColor"),
    ("animatemotion", "animateMotion"),
    ("animatetransform", "animateTransform"),
    ("clippath", "clipPath"),
    ("feblend", "feBlend"),
    ("fecolormatrix", "feColorMatrix"),
    ("fecomponenttransfer", "feComponentTransfer"),
    ("fecomposite", "feComposite"),
    ("feconvolvematrix", "feConvolveMatrix"),
    ("fediffuselighting", "feDiffuseLighting"),
    ("fedisplacementmap", "feDisplacementMap"),
    ("fedistantlight", "feDistantLight"),
    ("fedropshadow", "feDropShadow"),
    ("feflood", "feFlood"),
    ("fefunca", "feFuncA"),
    ("fefuncb", "feFuncB"),
    ("fefuncg", "feFuncG"),
    ("fefuncr", "feFuncR"),
    ("fegaussianblur", "feGaussianBlur"),
    ("feimage", "feImage"),
    ("femerge", "feMerge"),
    ("femergenode", "feMergeNode"),
    ("femorphology", "feMorphology"),
    ("feoffset", "feOffset"),
    ("fepointlight", "fePointLight"),
    ("fespecularlighting", "feSpecularLighting"),
    ("fespotlight", "feSpotLight"),
    ("fetile", "feTile"),
    ("feturbulence", "feTurbulence"),
    ("foreignobject", "foreignObject"),
    ("glyphref", "glyphRef"),
    ("lineargradient", "linearGradient"),
    ("radialgradient", "radialGradient"),
    ("textpath", "textPath"),
];

/// SVG attribute names that are not all-lowercase in the DOM.
const SVG_ATTR_FIXES: &[(&str, &str)] = &[
    ("attributename", "attributeName"),
    ("attributetype", "attributeType"),
    ("basefrequency", "baseFrequency"),
    ("baseprofile", "baseProfile"),
    ("calcmode", "calcMode"),
    ("clippathunits", "clipPathUnits"),
    ("diffuseconstant", "diffuseConstant"),
    ("edgemode", "edgeMode"),
    ("filterunits", "filterUnits"),
    ("glyphref", "glyphRef"),
    ("gradienttransform", "gradientTransform"),
    ("gradientunits", "gradientUnits"),
    ("kernelmatrix", "kernelMatrix"),
    ("kernelunitlength", "kernelUnitLength"),
    ("keypoints", "keyPoints"),
    ("keysplines", "keySplines"),
    ("keytimes", "keyTimes"),
    ("lengthadjust", "lengthAdjust"),
    ("limitingconeangle", "limitingConeAngle"),
    ("markerheight", "markerHeight"),
    ("markerunits", "markerUnits"),
    ("markerwidth", "markerWidth"),
    ("maskcontentunits", "maskContentUnits"),
    ("maskunits", "maskUnits"),
    ("numoctaves", "numOctaves"),
    ("pathlength", "pathLength"),
    ("patterncontentunits", "patternContentUnits"),
    ("patterntransform", "patternTransform"),
    ("patternunits", "patternUnits"),
    ("pointsatx", "pointsAtX"),
    ("pointsaty", "pointsAtY"),
    ("pointsatz", "pointsAtZ"),
    ("preservealpha", "preserveAlpha"),
    ("preserveaspectratio", "preserveAspectRatio"),
    ("primitiveunits", "primitiveUnits"),
    ("refx", "refX"),
    ("refy", "refY"),
    ("repeatcount", "repeatCount"),
    ("repeatdur", "repeatDur"),
    ("requiredextensions", "requiredExtensions"),
    ("requiredfeatures", "requiredFeatures"),
    ("specularconstant", "specularConstant"),
    ("specularexponent", "specularExponent"),
    ("spreadmethod", "spreadMethod"),
    ("startoffset", "startOffset"),
    ("stddeviation", "stdDeviation"),
    ("stitchtiles", "stitchTiles"),
    ("surfacescale", "surfaceScale"),
    ("systemlanguage", "systemLanguage"),
    ("tablevalues", "tableValues"),
    ("targetx", "targetX"),
    ("targety", "targetY"),
    ("textlength", "textLength"),
    ("viewbox", "viewBox"),
    ("viewtarget", "viewTarget"),
    ("xchannelselector", "xChannelSelector"),
    ("ychannelselector", "yChannelSelector"),
    ("zoomandpan", "zoomAndPan"),
];

/// Attributes that carry an XML namespace prefix in foreign content.
const FOREIGN_ATTR_FIXES: &[(&str, &str, &str)] = &[
    ("xlink:actuate", "xlink", "actuate"),
    ("xlink:arcrole", "xlink", "arcrole"),
    ("xlink:href", "xlink", "href"),
    ("xlink:role", "xlink", "role"),
    ("xlink:show", "xlink", "show"),
    ("xlink:title", "xlink", "title"),
    ("xlink:type", "xlink", "type"),
    ("xml:lang", "xml", "lang"),
    ("xml:space", "xml", "space"),
    ("xmlns", "", "xmlns"),
    ("xmlns:xlink", "xmlns", "xlink"),
];

const QUIRKS_PUBLIC_PREFIXES: &[&str] = &[
    "+//silmaril//dtd html pro v0r11 19970101//",
    "-//as//dtd html 3.0 aswedit + extensions//",
    "-//advasoft ltd//dtd html 3.0 aswedit + extensions//",
    "-//ietf//dtd html 2.0 level 1//",
    "-//ietf//dtd html 2.0 level 2//",
    "-//ietf//dtd html 2.0 strict level 1//",
    "-//ietf//dtd html 2.0 strict level 2//",
    "-//ietf//dtd html 2.0 strict//",
    "-//ietf//dtd html 2.0//",
    "-//ietf//dtd html 2.1e//",
    "-//ietf//dtd html 3.0//",
    "-//ietf//dtd html 3.2 final//",
    "-//ietf//dtd html 3.2//",
    "-//ietf//dtd html 3//",
    "-//ietf//dtd html level 0//",
    "-//ietf//dtd html level 1//",
    "-//ietf//dtd html level 2//",
    "-//ietf//dtd html level 3//",
    "-//ietf//dtd html strict level 0//",
    "-//ietf//dtd html strict level 1//",
    "-//ietf//dtd html strict level 2//",
    "-//ietf//dtd html strict level 3//",
    "-//ietf//dtd html strict//",
    "-//ietf//dtd html//",
    "-//metrius//dtd metrius presentational//",
    "-//microsoft//dtd internet explorer 2.0 html strict//",
    "-//microsoft//dtd internet explorer 2.0 html//",
    "-//microsoft//dtd internet explorer 2.0 tables//",
    "-//microsoft//dtd internet explorer 3.0 html strict//",
    "-//microsoft//dtd internet explorer 3.0 html//",
    "-//microsoft//dtd internet explorer 3.0 tables//",
    "-//netscape comm. corp.//dtd html//",
    "-//netscape comm. corp.//dtd strict html//",
    "-//o'reilly and associates//dtd html 2.0//",
    "-//o'reilly and associates//dtd html extended 1.0//",
    "-//o'reilly and associates//dtd html extended relaxed 1.0//",
    "-//sq//dtd html 2.0 hotmetal + extensions//",
    "-//softquad software//dtd hotmetal pro 6.0::19990601::extensions to html 4.0//",
    "-//softquad//dtd hotmetal pro 4.0::19971010::extensions to html 4.0//",
    "-//spyglass//dtd html 2.0 extended//",
    "-//sun microsystems corp.//dtd hotjava html//",
    "-//sun microsystems corp.//dtd hotjava strict html//",
    "-//w3c//dtd html 3 1995-03-24//",
    "-//w3c//dtd html 3.2 draft//",
    "-//w3c//dtd html 3.2 final//",
    "-//w3c//dtd html 3.2//",
    "-//w3c//dtd html 3.2s draft//",
    "-//w3c//dtd html 4.0 frameset//",
    "-//w3c//dtd html 4.0 transitional//",
    "-//w3c//dtd html experimental 19960712//",
    "-//w3c//dtd html experimental 970421//",
    "-//w3c//dtd w3 html//",
    "-//w3o//dtd w3 html 3.0//",
    "-//webtechs//dtd mozilla html 2.0//",
    "-//webtechs//dtd mozilla html//",
];

impl TreeBuilder {
    /// The tree construction dispatcher: decide whether this token goes
    /// through the current insertion mode or the foreign content rules.
    fn process(&mut self, t: Token) {
        let use_foreign = match self.adjusted_current_node() {
            None => false,
            Some(n) => {
                if self.sink.ns(n) == Ns::Html {
                    false
                } else if self.is_mathml_text_integration_point(n) {
                    // Only mglyph/malignmark stay foreign here; every
                    // other start tag, and character data, is handled
                    // by the current insertion mode.
                    match &t {
                        Token::StartTag { name, .. } => {
                            name == "mglyph" || name == "malignmark"
                        }
                        Token::Char(_) => false,
                        _ => true,
                    }
                } else if self.sink.ns(n) == Ns::MathMl
                    && self.sink.tag(n) == "annotation-xml"
                {
                    !matches!(&t, Token::StartTag { name, .. } if name == "svg")
                } else if self.is_html_integration_point(n) {
                    !matches!(&t, Token::StartTag { .. } | Token::Char(_))
                } else {
                    !matches!(t, Token::Eof)
                }
            }
        };
        if use_foreign {
            self.foreign(t);
        } else {
            self.by_mode(t);
        }
    }

    fn is_mathml_text_integration_point(&self, n: usize) -> bool {
        self.sink.ns(n) == Ns::MathMl
            && matches!(self.sink.tag(n), "mi" | "mo" | "mn" | "ms" | "mtext")
    }

    fn is_html_integration_point(&self, n: usize) -> bool {
        match self.sink.ns(n) {
            Ns::MathMl => {
                self.sink.tag(n) == "annotation-xml"
                    && self.sink.attr(n, "encoding").is_some_and(|e| {
                        e.eq_ignore_ascii_case("text/html")
                            || e.eq_ignore_ascii_case(
                                "application/xhtml+xml",
                            )
                    })
            }
            Ns::Svg => matches!(
                self.sink.tag(n),
                "foreignObject" | "desc" | "title"
            ),
            Ns::Html => false,
        }
    }

    fn by_mode(&mut self, t: Token) {
        match self.mode {
            Mode::Initial => self.m_initial(t),
            Mode::BeforeHtml => self.m_before_html(t),
            Mode::BeforeHead => self.m_before_head(t),
            Mode::InHead => self.m_in_head(t),
            Mode::InHeadNoscript => self.m_in_head_noscript(t),
            Mode::AfterHead => self.m_after_head(t),
            Mode::InBody => self.m_in_body(t),
            Mode::Text => self.m_text(t),
            Mode::InTable => self.m_in_table(t),
            Mode::InTableText => self.m_in_table_text(t),
            Mode::InCaption => self.m_in_caption(t),
            Mode::InColumnGroup => self.m_in_column_group(t),
            Mode::InTableBody => self.m_in_table_body(t),
            Mode::InRow => self.m_in_row(t),
            Mode::InCell => self.m_in_cell(t),
            Mode::InSelect => self.m_in_select(t),
            Mode::InSelectInTable => self.m_in_select_in_table(t),
            Mode::InTemplate => self.m_in_template(t),
            Mode::AfterBody => self.m_after_body(t),
            Mode::InFrameset => self.m_in_frameset(t),
            Mode::AfterFrameset => self.m_after_frameset(t),
            Mode::AfterAfterBody => self.m_after_after_body(t),
            Mode::AfterAfterFrameset => self.m_after_after_frameset(t),
        }
    }

    fn stop(&mut self) {
        self.done = true;
    }

    // ---- Initial ------------------------------------------------------

    fn m_initial(&mut self, t: Token) {
        match t {
            Token::Char(c) if is_ws(c) => {}
            Token::Comment(c) => {
                let n = self.sink.push(NodeData::Comment(c));
                let doc = self.sink.document;
                self.sink.append(doc, n);
            }
            Token::Doctype { name, public_id, system_id, force_quirks } => {
                let nm = name.clone().unwrap_or_default();
                let pid = public_id.clone().unwrap_or_default();
                let sid = system_id.clone().unwrap_or_default();
                let n = self.sink.push(NodeData::Doctype {
                    name: nm.clone(),
                    public_id: pid.clone(),
                    system_id: sid.clone(),
                });
                let doc = self.sink.document;
                self.sink.append(doc, n);
                self.sink.quirks =
                    quirks_for(&nm, &pid, &sid, force_quirks, public_id.is_some(), system_id.is_some());
                self.mode = Mode::BeforeHtml;
            }
            other => {
                self.sink.quirks = Quirks::Quirks;
                self.mode = Mode::BeforeHtml;
                self.by_mode(other);
            }
        }
    }

    // ---- BeforeHtml ---------------------------------------------------

    fn m_before_html(&mut self, t: Token) {
        match t {
            Token::Doctype { .. } => {}
            Token::Comment(c) => {
                let n = self.sink.push(NodeData::Comment(c));
                let doc = self.sink.document;
                self.sink.append(doc, n);
            }
            Token::Char(c) if is_ws(c) => {}
            Token::StartTag { ref name, ref attrs, .. } if name == "html" => {
                let n = self.create(Ns::Html, "html", attrs.clone());
                let doc = self.sink.document;
                self.sink.append(doc, n);
                self.open.push(n);
                self.mode = Mode::BeforeHead;
            }
            Token::EndTag { ref name }
                if !matches!(name.as_str(), "head" | "body" | "html" | "br") =>
            {
            }
            other => {
                let n = self.create(Ns::Html, "html", Vec::new());
                let doc = self.sink.document;
                self.sink.append(doc, n);
                self.open.push(n);
                self.mode = Mode::BeforeHead;
                self.by_mode(other);
            }
        }
    }

    // ---- BeforeHead ---------------------------------------------------

    fn m_before_head(&mut self, t: Token) {
        match t {
            Token::Char(c) if is_ws(c) => {}
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "head" => {
                let n = self.insert_element(Ns::Html, "head", attrs.clone());
                self.head = Some(n);
                self.mode = Mode::InHead;
            }
            Token::EndTag { ref name }
                if !matches!(name.as_str(), "head" | "body" | "html" | "br") =>
            {
            }
            other => {
                let n = self.insert_element(Ns::Html, "head", Vec::new());
                self.head = Some(n);
                self.mode = Mode::InHead;
                self.by_mode(other);
            }
        }
    }

    // ---- InHead -------------------------------------------------------

    fn m_in_head(&mut self, t: Token) {
        match t {
            Token::Char(c) if is_ws(c) => self.insert_text(&c.to_string()),
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(
                    name.as_str(),
                    "base" | "basefont" | "bgsound" | "link"
                ) =>
            {
                self.insert_element(Ns::Html, name, attrs.clone());
                self.open.pop();
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "meta" => {
                self.insert_element(Ns::Html, "meta", attrs.clone());
                self.open.pop();
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "title" => {
                self.rcdata(name.clone(), attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "noscript" && self.scripting =>
            {
                self.rawtext(name.clone(), attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(name.as_str(), "noframes" | "style") =>
            {
                self.rawtext(name.clone(), attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "noscript" && !self.scripting =>
            {
                self.insert_element(Ns::Html, "noscript", attrs.clone());
                self.mode = Mode::InHeadNoscript;
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "script" => {
                self.insert_element(Ns::Html, "script", attrs.clone());
                self.tok.state = TState::ScriptData;
                self.original_mode = self.mode;
                self.mode = Mode::Text;
            }
            Token::EndTag { ref name } if name == "head" => {
                self.open.pop();
                self.mode = Mode::AfterHead;
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "template" =>
            {
                self.insert_element(Ns::Html, "template", attrs.clone());
                self.push_marker();
                self.frameset_ok = false;
                self.mode = Mode::InTemplate;
                self.template_modes.push(Mode::InTemplate);
            }
            Token::EndTag { ref name } if name == "template" => {
                if !self.open.iter().any(|&n| {
                    self.sink.is_html_element(n, "template")
                }) {
                    return;
                }
                self.generate_implied_end_thoroughly();
                self.pop_until_html("template");
                self.clear_active_to_marker();
                self.template_modes.pop();
                self.reset_insertion_mode();
            }
            Token::EndTag { ref name }
                if matches!(name.as_str(), "body" | "html" | "br") =>
            {
                self.open.pop();
                self.mode = Mode::AfterHead;
                self.by_mode(t);
            }
            Token::StartTag { ref name, .. } if name == "head" => {}
            Token::EndTag { .. } => {}
            other => {
                self.open.pop();
                self.mode = Mode::AfterHead;
                self.by_mode(other);
            }
        }
    }

    fn rcdata(&mut self, name: String, attrs: Vec<(AttrName, String)>) {
        self.insert_element(Ns::Html, &name, attrs);
        self.tok.state = TState::Rcdata;
        self.original_mode = self.mode;
        self.mode = Mode::Text;
    }

    fn rawtext(&mut self, name: String, attrs: Vec<(AttrName, String)>) {
        self.insert_element(Ns::Html, &name, attrs);
        self.tok.state = TState::Rawtext;
        self.original_mode = self.mode;
        self.mode = Mode::Text;
    }

    // ---- InHeadNoscript ----------------------------------------------

    fn m_in_head_noscript(&mut self, t: Token) {
        match t {
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::EndTag { ref name } if name == "noscript" => {
                self.open.pop();
                self.mode = Mode::InHead;
            }
            Token::Char(c) if is_ws(c) => self.m_in_head(Token::Char(c)),
            Token::Comment(_) => self.m_in_head(t),
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "basefont" | "bgsound" | "link" | "meta" | "noframes"
                        | "style"
                ) =>
            {
                self.m_in_head(t)
            }
            Token::EndTag { ref name } if name == "br" => {
                self.open.pop();
                self.mode = Mode::InHead;
                self.by_mode(t);
            }
            Token::StartTag { ref name, .. }
                if matches!(name.as_str(), "head" | "noscript") => {}
            Token::EndTag { .. } => {}
            other => {
                self.open.pop();
                self.mode = Mode::InHead;
                self.by_mode(other);
            }
        }
    }

    // ---- AfterHead ----------------------------------------------------

    fn m_after_head(&mut self, t: Token) {
        match t {
            Token::Char(c) if is_ws(c) => self.insert_text(&c.to_string()),
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, .. } if name == "html" => {
                self.m_in_body(t)
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "body" => {
                self.insert_element(Ns::Html, "body", attrs.clone());
                self.frameset_ok = false;
                self.mode = Mode::InBody;
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "frameset" =>
            {
                self.insert_element(Ns::Html, "frameset", attrs.clone());
                self.mode = Mode::InFrameset;
            }
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "base" | "basefont" | "bgsound" | "link" | "meta"
                        | "noframes" | "script" | "style" | "template"
                        | "title"
                ) =>
            {
                if let Some(h) = self.head {
                    self.open.push(h);
                    self.m_in_head(t);
                    if let Some(pos) = self.open.iter().rposition(|&n| n == h) {
                        self.open.remove(pos);
                    }
                }
            }
            Token::EndTag { ref name } if name == "template" => {
                self.m_in_head(t)
            }
            Token::EndTag { ref name }
                if matches!(name.as_str(), "body" | "html" | "br") =>
            {
                self.insert_element(Ns::Html, "body", Vec::new());
                self.mode = Mode::InBody;
                self.by_mode(t);
            }
            Token::StartTag { ref name, .. } if name == "head" => {}
            Token::EndTag { .. } => {}
            other => {
                self.insert_element(Ns::Html, "body", Vec::new());
                self.mode = Mode::InBody;
                self.by_mode(other);
            }
        }
    }

    // ---- Text ---------------------------------------------------------

    fn m_text(&mut self, t: Token) {
        match t {
            Token::Char(c) => {
                if self.ignore_lf && c == '\n' {
                    self.ignore_lf = false;
                    return;
                }
                self.ignore_lf = false;
                self.insert_text(&c.to_string());
            }
            Token::Eof => {
                self.open.pop();
                self.mode = self.original_mode;
                self.by_mode(Token::Eof);
            }
            Token::EndTag { .. } => {
                self.open.pop();
                self.mode = self.original_mode;
            }
            _ => {}
        }
    }

    // ---- InBody -------------------------------------------------------

    fn m_in_body(&mut self, t: Token) {
        match t {
            Token::Char('\0') => {}
            Token::Char(c) => {
                if self.ignore_lf {
                    self.ignore_lf = false;
                    if c == '\n' {
                        return;
                    }
                }
                self.reconstruct_active();
                self.insert_text(&c.to_string());
                if !is_ws(c) {
                    self.frameset_ok = false;
                }
            }
            Token::Comment(c) => self.insert_comment(&c),
            Token::Doctype { .. } => {}
            Token::StartTag { ref name, ref attrs, .. } if name == "html" => {
                if self.open.iter().any(|&n| {
                    self.sink.is_html_element(n, "template")
                }) {
                    return;
                }
                let root = self.open[0];
                self.merge_attrs(root, attrs);
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
            Token::StartTag { ref name, ref attrs, .. } if name == "body" => {
                let Some(&second) = self.open.get(1) else { return };
                if !self.sink.is_html_element(second, "body")
                    || self.open.len() == 1
                    || self.open.iter().any(|&n| {
                        self.sink.is_html_element(n, "template")
                    })
                {
                    return;
                }
                self.frameset_ok = false;
                self.merge_attrs(second, attrs);
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "frameset" =>
            {
                let Some(&second) = self.open.get(1) else { return };
                if self.open.len() == 1
                    || !self.sink.is_html_element(second, "body")
                    || !self.frameset_ok
                {
                    return;
                }
                if let Some(p) = self.sink.nodes[second].parent {
                    let _ = p;
                    self.sink.detach(second);
                }
                self.open.truncate(1);
                self.insert_element(Ns::Html, "frameset", attrs.clone());
                self.mode = Mode::InFrameset;
            }
            Token::Eof => {
                if !self.template_modes.is_empty() {
                    self.m_in_template(Token::Eof);
                    return;
                }
                self.stop();
            }
            Token::EndTag { ref name } if name == "body" => {
                if !self.in_scope("body") {
                    return;
                }
                self.mode = Mode::AfterBody;
            }
            Token::EndTag { ref name } if name == "html" => {
                if !self.in_scope("body") {
                    return;
                }
                self.mode = Mode::AfterBody;
                self.by_mode(Token::EndTag { name: "html".to_string() });
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(
                    name.as_str(),
                    "address" | "article" | "aside" | "blockquote"
                        | "center" | "details" | "dialog" | "dir" | "div"
                        | "dl" | "fieldset" | "figcaption" | "figure"
                        | "footer" | "header" | "hgroup" | "main" | "menu"
                        | "nav" | "ol" | "p" | "search" | "section"
                        | "summary" | "ul"
                ) =>
            {
                if self.in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(Ns::Html, name, attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(
                    name.as_str(),
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                ) =>
            {
                if self.in_button_scope("p") {
                    self.close_p();
                }
                let cur = self.current();
                if self.sink.ns(cur) == Ns::Html
                    && matches!(
                        self.sink.tag(cur),
                        "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                    )
                {
                    self.open.pop();
                }
                self.insert_element(Ns::Html, name, attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(name.as_str(), "pre" | "listing") =>
            {
                if self.in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(Ns::Html, name, attrs.clone());
                self.ignore_lf = true;
                self.frameset_ok = false;
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "form" => {
                let has_template = self.open.iter().any(|&n| {
                    self.sink.is_html_element(n, "template")
                });
                if self.form.is_some() && !has_template {
                    return;
                }
                if self.in_button_scope("p") {
                    self.close_p();
                }
                let n = self.insert_element(Ns::Html, "form", attrs.clone());
                if !has_template {
                    self.form = Some(n);
                }
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "li" => {
                self.frameset_ok = false;
                for i in (0..self.open.len()).rev() {
                    let n = self.open[i];
                    if self.sink.is_html_element(n, "li") {
                        self.generate_implied_end(Some("li"));
                        self.pop_until_html("li");
                        break;
                    }
                    if self.sink.ns(n) == Ns::Html
                        && SPECIAL.contains(&self.sink.tag(n))
                        && !matches!(
                            self.sink.tag(n),
                            "address" | "div" | "p"
                        )
                    {
                        break;
                    }
                }
                if self.in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(Ns::Html, "li", attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(name.as_str(), "dd" | "dt") =>
            {
                self.frameset_ok = false;
                for i in (0..self.open.len()).rev() {
                    let n = self.open[i];
                    let tag = self.sink.tag(n).to_string();
                    if self.sink.ns(n) == Ns::Html
                        && (tag == "dd" || tag == "dt")
                    {
                        self.generate_implied_end(Some(&tag));
                        self.pop_until_html(&tag);
                        break;
                    }
                    if self.sink.ns(n) == Ns::Html
                        && SPECIAL.contains(&tag.as_str())
                        && !matches!(tag.as_str(), "address" | "div" | "p")
                    {
                        break;
                    }
                }
                if self.in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(Ns::Html, name, attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "plaintext" =>
            {
                if self.in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(Ns::Html, "plaintext", attrs.clone());
                self.tok.state = TState::Plaintext;
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "button" => {
                if self.in_scope("button") {
                    self.generate_implied_end(None);
                    self.pop_until_html("button");
                }
                self.reconstruct_active();
                self.insert_element(Ns::Html, "button", attrs.clone());
                self.frameset_ok = false;
            }
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "address" | "article" | "aside" | "blockquote"
                        | "button" | "center" | "details" | "dialog"
                        | "dir" | "div" | "dl" | "fieldset" | "figcaption"
                        | "figure" | "footer" | "header" | "hgroup"
                        | "listing" | "main" | "menu" | "nav" | "ol"
                        | "pre" | "search" | "section" | "summary" | "ul"
                ) =>
            {
                if !self.in_scope(name) {
                    return;
                }
                self.generate_implied_end(None);
                self.pop_until_html(name);
            }
            Token::EndTag { ref name } if name == "form" => {
                let has_template = self.open.iter().any(|&n| {
                    self.sink.is_html_element(n, "template")
                });
                if !has_template {
                    let node = self.form.take();
                    let Some(node) = node else { return };
                    if !self.open.contains(&node) || !self.in_scope("form") {
                        return;
                    }
                    self.generate_implied_end(None);
                    if let Some(pos) =
                        self.open.iter().rposition(|&n| n == node)
                    {
                        self.open.remove(pos);
                    }
                } else {
                    if !self.in_scope("form") {
                        return;
                    }
                    self.generate_implied_end(None);
                    self.pop_until_html("form");
                }
            }
            Token::EndTag { ref name } if name == "p" => {
                if !self.in_button_scope("p") {
                    // A stray </p> stands in for a whole <p></p>. Send the
                    // start tag back through the dispatcher rather than
                    // inserting it here: inside foreign content it is one
                    // of the tags that breaks out, so `<svg></p>` puts the
                    // <p> beside the <svg>, not in it.
                    self.process(Token::StartTag {
                        name: "p".into(),
                        attrs: Vec::new(),
                        self_closing: false,
                    });
                }
                self.close_p();
            }
            Token::EndTag { ref name } if name == "li" => {
                if !self.in_list_item_scope("li") {
                    return;
                }
                self.generate_implied_end(Some("li"));
                self.pop_until_html("li");
            }
            Token::EndTag { ref name }
                if matches!(name.as_str(), "dd" | "dt") =>
            {
                if !self.in_scope(name) {
                    return;
                }
                self.generate_implied_end(Some(name));
                self.pop_until_html(name);
            }
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                ) =>
            {
                const HS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];
                if !self.in_scope_any(HS) {
                    return;
                }
                self.generate_implied_end(None);
                self.pop_until_any(HS);
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "a" => {
                // an existing <a> in the active list is adopted out first
                let mut existing = None;
                for i in (0..self.active.len()).rev() {
                    match self.active[i] {
                        Formatting::Marker => break,
                        Formatting::Element(e) => {
                            if self.sink.is_html_element(e, "a") {
                                existing = Some(e);
                                break;
                            }
                        }
                    }
                }
                if let Some(e) = existing {
                    self.adoption_agency("a");
                    if let Some(ai) = self.active_index(e) {
                        self.active.remove(ai);
                    }
                    if let Some(oi) = self.open.iter().position(|&n| n == e) {
                        self.open.remove(oi);
                    }
                }
                self.reconstruct_active();
                let n = self.insert_element(Ns::Html, "a", attrs.clone());
                self.push_active(n);
            }
            Token::StartTag { ref name, ref attrs, .. }
                if FORMATTING.contains(&name.as_str()) && name != "a"
                    && name != "nobr" =>
            {
                self.reconstruct_active();
                let n = self.insert_element(Ns::Html, name, attrs.clone());
                self.push_active(n);
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "nobr" => {
                self.reconstruct_active();
                if self.in_scope("nobr") {
                    // The open <nobr> can be in scope yet be hidden behind
                    // a marker in the active list — an unclosed <marquee>
                    // in a table does that. The agency finds nothing and
                    // hands over to the plain end-tag close.
                    if !self.adoption_agency("nobr") {
                        self.any_other_end_tag("nobr");
                    }
                    self.reconstruct_active();
                }
                let n = self.insert_element(Ns::Html, "nobr", attrs.clone());
                self.push_active(n);
            }
            Token::EndTag { ref name }
                if FORMATTING.contains(&name.as_str()) =>
            {
                if !self.adoption_agency(name) {
                    self.any_other_end_tag(name);
                }
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(
                    name.as_str(),
                    "applet" | "marquee" | "object"
                ) =>
            {
                self.reconstruct_active();
                self.insert_element(Ns::Html, name, attrs.clone());
                self.push_marker();
                self.frameset_ok = false;
            }
            Token::EndTag { ref name }
                if matches!(
                    name.as_str(),
                    "applet" | "marquee" | "object"
                ) =>
            {
                if !self.in_scope(name) {
                    return;
                }
                self.generate_implied_end(None);
                self.pop_until_html(name);
                self.clear_active_to_marker();
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "table" => {
                if self.sink.quirks != Quirks::Quirks
                    && self.in_button_scope("p")
                {
                    self.close_p();
                }
                self.insert_element(Ns::Html, "table", attrs.clone());
                self.frameset_ok = false;
                self.mode = Mode::InTable;
            }
            Token::EndTag { ref name } if name == "br" => {
                // "act as if this was a br start tag token" — literally,
                // dispatcher and all, so it breaks out of foreign content.
                self.process(Token::StartTag {
                    name: "br".into(),
                    attrs: Vec::new(),
                    self_closing: false,
                });
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(
                    name.as_str(),
                    "area" | "br" | "embed" | "img" | "keygen" | "wbr"
                ) =>
            {
                self.reconstruct_active();
                self.insert_element(Ns::Html, name, attrs.clone());
                self.open.pop();
                self.frameset_ok = false;
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "input" => {
                self.reconstruct_active();
                self.insert_element(Ns::Html, "input", attrs.clone());
                self.open.pop();
                let hidden = attrs.iter().any(|(k, v)| {
                    k.prefix.is_none()
                        && k.local == "type"
                        && v.eq_ignore_ascii_case("hidden")
                });
                if !hidden {
                    self.frameset_ok = false;
                }
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(name.as_str(), "param" | "source" | "track") =>
            {
                self.insert_element(Ns::Html, name, attrs.clone());
                self.open.pop();
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "hr" => {
                if self.in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element(Ns::Html, "hr", attrs.clone());
                self.open.pop();
                self.frameset_ok = false;
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "image" => {
                // the spec's one deliberate rename
                let a = attrs.clone();
                let _ = name;
                self.m_in_body(Token::StartTag {
                    name: "img".to_string(),
                    attrs: a,
                    self_closing: false,
                });
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "textarea" =>
            {
                self.insert_element(Ns::Html, "textarea", attrs.clone());
                self.ignore_lf = true;
                self.tok.state = TState::Rcdata;
                self.original_mode = self.mode;
                self.frameset_ok = false;
                self.mode = Mode::Text;
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "xmp" => {
                if self.in_button_scope("p") {
                    self.close_p();
                }
                self.reconstruct_active();
                self.frameset_ok = false;
                self.rawtext(name.clone(), attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "iframe" =>
            {
                self.frameset_ok = false;
                self.rawtext(name.clone(), attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if name == "noembed"
                    || (name == "noscript" && self.scripting) =>
            {
                self.rawtext(name.clone(), attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. } if name == "select" => {
                self.reconstruct_active();
                self.insert_element(Ns::Html, "select", attrs.clone());
                self.frameset_ok = false;
                self.mode = match self.mode {
                    Mode::InTable
                    | Mode::InCaption
                    | Mode::InTableBody
                    | Mode::InRow
                    | Mode::InCell => Mode::InSelectInTable,
                    _ => Mode::InSelect,
                };
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(name.as_str(), "optgroup" | "option") =>
            {
                if self.sink.is_html_element(self.current(), "option") {
                    self.open.pop();
                }
                self.reconstruct_active();
                self.insert_element(Ns::Html, name, attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(name.as_str(), "rb" | "rtc") =>
            {
                if self.in_scope("ruby") {
                    self.generate_implied_end(None);
                }
                self.insert_element(Ns::Html, name, attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, .. }
                if matches!(name.as_str(), "rp" | "rt") =>
            {
                if self.in_scope("ruby") {
                    self.generate_implied_end(Some("rtc"));
                }
                self.insert_element(Ns::Html, name, attrs.clone());
            }
            Token::StartTag { ref name, ref attrs, self_closing }
                if name == "math" =>
            {
                self.reconstruct_active();
                let mut a = adjust_mathml_attrs(attrs.clone());
                a = adjust_foreign_attrs(a);
                self.insert_element(Ns::MathMl, "math", a);
                if self_closing {
                    self.open.pop();
                }
            }
            Token::StartTag { ref name, ref attrs, self_closing }
                if name == "svg" =>
            {
                self.reconstruct_active();
                let mut a = adjust_svg_attrs(attrs.clone());
                a = adjust_foreign_attrs(a);
                self.insert_element(Ns::Svg, "svg", a);
                if self_closing {
                    self.open.pop();
                }
            }
            Token::StartTag { ref name, .. }
                if matches!(
                    name.as_str(),
                    "caption" | "col" | "colgroup" | "frame" | "head"
                        | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr"
                ) => {}
            Token::StartTag { ref name, ref attrs, .. } => {
                self.reconstruct_active();
                self.insert_element(Ns::Html, name, attrs.clone());
            }
            Token::EndTag { ref name } => {
                self.any_other_end_tag(name);
            }
        }
    }

    fn any_other_end_tag(&mut self, name: &str) {
        for i in (0..self.open.len()).rev() {
            let n = self.open[i];
            if self.sink.ns(n) == Ns::Html && self.sink.tag(n) == name {
                self.generate_implied_end(Some(name));
                self.open.truncate(i);
                return;
            }
            if self.sink.ns(n) == Ns::Html
                && SPECIAL.contains(&self.sink.tag(n))
            {
                return;
            }
        }
    }

    fn merge_attrs(&mut self, node: usize, attrs: &[(AttrName, String)]) {
        if let NodeData::Element { attrs: existing, .. } =
            &mut self.sink.nodes[node].data
        {
            for (k, v) in attrs {
                if !existing.iter().any(|(k2, _)| k2 == k) {
                    existing.push((k.clone(), v.clone()));
                }
            }
        }
    }
}

fn quirks_for(
    name: &str, public: &str, system: &str, force: bool,
    _has_public: bool, has_system: bool,
) -> Quirks {
    if force || name != "html" {
        return Quirks::Quirks;
    }
    let p = public.to_ascii_lowercase();
    let s = system.to_ascii_lowercase();
    if p == "-//w3o//dtd w3 html strict 3.0//en//"
        || p == "-/w3c/dtd html 4.0 transitional/en"
        || p == "html"
        || s == "http://www.ibm.com/data/dtd/v11/ibmxhtml1-transitional.dtd"
    {
        return Quirks::Quirks;
    }
    if QUIRKS_PUBLIC_PREFIXES.iter().any(|q| p.starts_with(q)) {
        return Quirks::Quirks;
    }
    if !has_system
        && (p.starts_with("-//w3c//dtd html 4.01 frameset//")
            || p.starts_with("-//w3c//dtd html 4.01 transitional//"))
    {
        return Quirks::Quirks;
    }
    if p.starts_with("-//w3c//dtd xhtml 1.0 frameset//")
        || p.starts_with("-//w3c//dtd xhtml 1.0 transitional//")
    {
        return Quirks::LimitedQuirks;
    }
    if has_system
        && (p.starts_with("-//w3c//dtd html 4.01 frameset//")
            || p.starts_with("-//w3c//dtd html 4.01 transitional//"))
    {
        return Quirks::LimitedQuirks;
    }
    Quirks::NoQuirks
}

fn adjust_svg_attrs(
    attrs: Vec<(AttrName, String)>,
) -> Vec<(AttrName, String)> {
    attrs
        .into_iter()
        .map(|(k, v)| {
            if k.prefix.is_none() {
                if let Some((_, fixed)) =
                    SVG_ATTR_FIXES.iter().find(|(a, _)| *a == k.local)
                {
                    return (AttrName::local(*fixed), v);
                }
            }
            (k, v)
        })
        .collect()
}

fn adjust_mathml_attrs(
    attrs: Vec<(AttrName, String)>,
) -> Vec<(AttrName, String)> {
    attrs
        .into_iter()
        .map(|(k, v)| {
            if k.prefix.is_none() && k.local == "definitionurl" {
                (AttrName::local("definitionURL"), v)
            } else {
                (k, v)
            }
        })
        .collect()
}

fn adjust_foreign_attrs(
    attrs: Vec<(AttrName, String)>,
) -> Vec<(AttrName, String)> {
    attrs
        .into_iter()
        .map(|(k, v)| {
            if k.prefix.is_some() {
                return (k, v);
            }
            match FOREIGN_ATTR_FIXES.iter().find(|(a, _, _)| *a == k.local) {
                Some((_, prefix, local)) => (
                    AttrName {
                        prefix: if prefix.is_empty() {
                            None
                        } else {
                            Some((*prefix).to_string())
                        },
                        local: (*local).to_string(),
                    },
                    v,
                ),
                None => (k, v),
            }
        })
        .collect()
}
