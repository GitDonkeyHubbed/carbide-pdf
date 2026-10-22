            };
            let Some(found) = scan.uses_in(glyph.as_reference().ok(), stream)? else {
                return Ok(None);
            };
            uses.extend(found);
        }
        Ok(Some(uses))
    }

    /// `resources` with its categories as direct dictionaries, for comparing with `keep_named`.
    fn with_categories_resolved(&self, resources: &Dictionary) -> Dictionary {
        resources
            .iter()
            .map(|(key, value)| match self.resolve(value) {
                Object::Dictionary(entries) if NAMED_RESOURCES.contains(&key.as_slice()) => {
                    (key.clone(), Object::Dictionary(entries))
                }
                _ => (key.clone(), value.clone()),
            })
            .filter(|(_, value)| !matches!(value, Object::Dictionary(d) if d.is_empty()))
            .collect()
    }
}

/// What [`Pdf::trim_resources`] changes, worked out by [`Pdf::plan_resource_trim`].
pub(crate) struct ResourceTrim {
    /// The resource dictionary each page that needs a new one gets.
    pages: Vec<(ObjectId, Dictionary)>,
    /// The resources each form, pattern, Type 3 font and annotation appearance with resources of
    /// its own is left with, where they change.
    nested: HashMap<ObjectId, Dictionary>,
}

impl ResourceTrim {
    /// Takes the references to `ids` out of the resources this gives, as
    /// [`Pdf::drop_references_to`] takes them out of the document.
    pub(crate) fn drop_references_to(&mut self, ids: &HashSet<ObjectId>) {
        let pages = self.pages.iter_mut().map(|(_, resources)| resources);
        for resources in pages.chain(self.nested.values_mut()) {
            strip_dict_references(resources, ids);
        }
    }
}

/// Removes the array items and dictionary entries that refer to `ids` from `object` and
/// everything written straight into it.
fn strip_references(object: &mut Object, ids: &HashSet<ObjectId>) {
    match object {
        Object::Array(items) => {
            items.retain(|item| !item.as_reference().is_ok_and(|id| ids.contains(&id)));
            items
                .iter_mut()
                .for_each(|item| strip_references(item, ids));
        }
        Object::Dictionary(dict) => strip_dict_references(dict, ids),
        Object::Stream(stream) => strip_dict_references(&mut stream.dict, ids),
        _ => {}
    }
}

/// [`strip_references`] for a dictionary.
fn strip_dict_references(dict: &mut Dictionary, ids: &HashSet<ObjectId>) {
    let doomed: Vec<Vec<u8>> = dict
        .iter()
        .filter(|(_, value)| value.as_reference().is_ok_and(|id| ids.contains(&id)))
        .map(|(key, _)| key.clone())
        .collect();
    for key in doomed {
        dict.remove(&key);
    }
    dict.iter_mut()
        .for_each(|(_, value)| strip_references(value, ids));
}

/// The resources each content stream uses, found once per stream however many pages draw it,
/// each page's streams decoded within the budget of one page.
struct NameScan {
    budget: DecodeBudget,
    /// How many more different names the page being read may use (see [`MAX_NAMES`]).
    names_left: usize,
    names: HashMap<ObjectId, Option<Uses>>,
    /// What the content of pages split over several streams uses, by the streams, in order.
    joined: HashMap<Vec<ObjectId>, Option<Uses>>,
    /// Forms, patterns and fonts whose own resources were already trimmed.
    trimmed: HashSet<ObjectId>,
}

impl NameScan {
    /// The resources a page's content, the streams `streams`, uses; `None` when one cannot be
    /// decoded, and an error when decoding them would go past the budget. The streams are read
    /// as one, as the page's content is: an operator can take its operands from the stream
    /// before its own, so what one stream uses on its own is not what it uses on a page.
    fn page_uses(&mut self, doc: &Document, streams: Vec<ObjectId>) -> Result<Option<Uses>> {
        if let [id] = streams[..] {
            let Ok(content) = doc.get_object(id).and_then(Object::as_stream) else {
                return Ok(None);
            };
            return self.uses_in(Some(id), content);
        }
        if let Some(found) = self.joined.get(&streams) {
            let found = found.clone();
            self.count(&found)?;
            return Ok(found);
        }
        let mut joined = Vec::new();
        let mut readable = true;
        for id in &streams {
            let Ok(content) = doc.get_object(*id).and_then(Object::as_stream) else {
                readable = false;
                break;
            };
            match self.budget.stream(content) {
                Ok(mut plain) => {
                    joined.append(&mut plain);
                    joined.push(b'\n');
                }
                Err(error @ Error::TooLarge(_)) => return Err(error),
                Err(_) => {
                    readable = false;
                    break;
                }
            }
        }
        let found = if readable {
            Some(uses_in(&joined, &mut self.names_left)?)
        } else {
            None
        };
        self.joined.insert(streams, found.clone());
        Ok(found)
    }

    /// The resources `stream` (object `id`, when it is one of its own) uses; `None` when it
    /// cannot be decoded, and an error when decoding it would go past the budget.
    fn uses_in(&mut self, id: Option<ObjectId>, stream: &lopdf::Stream) -> Result<Option<Uses>> {
        if let Some(found) = id.and_then(|id| self.names.get(&id)) {
            let found = found.clone();
            self.count(&found)?;
            return Ok(found);
        }
        let found = match self.budget.stream(stream) {
            Ok(plain) => Some(uses_in(&plain, &mut self.names_left)?),
            Err(error @ Error::TooLarge(_)) => return Err(error),
            Err(_) => None,
        };
        if let Some(id) = id {
            self.names.insert(id, found.clone());
        }
        Ok(found)
    }

    /// Counts the names in `found`, found before, against those the page being read may use: the
    /// copy of them it is given takes as much memory as reading them did.
    fn count(&mut self, found: &Option<Uses>) -> Result<()> {
        let used = found.as_ref().map_or(0, HashSet::len);
        self.names_left = self
            .names_left
            .checked_sub(used)
            .ok_or_else(|| Error::TooLarge(messages::TOO_COMPLEX.into()))?;
        Ok(())
    }
}

/// The area a widget covers on its page, in user space; `None` for an invisible one.
pub(crate) fn widget_area(widget: &Dictionary) -> Option<[f32; 4]> {
    let rect = widget.get(b"Rect").and_then(Object::as_array).ok()?;
    let values: Vec<f32> = rect.iter().filter_map(|v| v.as_float().ok()).collect();
    let [x0, y0, x1, y1] = values[..] else {
        return None;
    };
    let area = [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)];
    (area[2] > area[0] && area[3] > area[1]).then_some(area)
}

/// The resources `content` uses: the names given to the operators that look a resource up, each
/// in the category that operator looks in. Names inside an inline image's dictionary count as
/// colour spaces, and every name given to a marked-content operator as properties.
///
/// `left` is how many more different names may be kept, and goes down by those this content
/// uses; past it, or with more different names before one operator than [`MAX_NAMES`], this
/// fails with [`Error::TooLarge`].
fn uses_in(content: &[u8], left: &mut usize) -> Result<Uses> {
    let mut uses = HashSet::new();
    // Which operand a name is, or how often it is given, does not matter: each is kept once, so
    // a name given millions of times before an operator takes no more memory than one.
    let mut operands: HashSet<&[u8]> = HashSet::new();
    let mut over = false;
    tokens(content, |token| match token {
        _ if over => {}
        Token::Name(written) => {
            over = operands.insert(written) && operands.len() > MAX_NAMES;
        }
        Token::Operator(operator) => {
            let category = match operator {
                b"Tf" => Some(FONT),
                b"Do" => Some(XOBJECT),
                b"gs" => Some(EXT_G_STATE),
                b"cs" | b"CS" | b"ID" => Some(COLOR_SPACE),
                b"scn" | b"SCN" => Some(PATTERN),
                b"sh" => Some(SHADING),
