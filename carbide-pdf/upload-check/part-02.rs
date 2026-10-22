        &self,
        page: ObjectId,
        resources: &Dictionary,
        scan: &mut NameScan,
    ) -> Result<Option<Uses>> {
        let streams = self.doc.get_page_contents(page);
        let Some(mut uses) = scan.page_uses(&self.doc, streams)? else {
            return Ok(None);
        };
        for id in self.appearances_on(page) {
            let Ok(Object::Stream(appearance)) = self.doc.get_object(id) else {
                continue;
            };
            if self.has_own_resources(&appearance.dict) {
                continue;
            }
            let Some(found) = scan.uses_in(Some(id), appearance)? else {
                return Ok(None);
            };
            uses.extend(found);
        }
        self.scope_uses(uses, resources, scan)
    }

    /// Whether `dict` has a resource dictionary of its own. An entry that is null, refers to
    /// nothing, or is not a dictionary counts as absent, so the content draws with the resources
    /// of what encloses it.
    fn has_own_resources(&self, dict: &Dictionary) -> bool {
        dict.get(b"Resources")
            .ok()
            .and_then(|r| self.resolved(r))
            .is_some_and(|r| matches!(r, Object::Dictionary(_)))
    }

    /// `uses`, the resources some content uses directly, together with what the forms, patterns
    /// and Type 3 fonts among them that have no resources of their own use from `resources`.
    fn scope_uses(
        &self,
        mut uses: Uses,
        resources: &Dictionary,
        scan: &mut NameScan,
    ) -> Result<Option<Uses>> {
        // Resources that are drawn without resources of their own use the enclosing ones.
        let mut followed = HashSet::new();
        loop {
            let mut more = Vec::new();
            for category in NAMED_RESOURCES {
                let Object::Dictionary(entries) = resources
                    .get(category)
                    .map(|o| self.resolve(o))
                    .unwrap_or(Object::Null)
                else {
                    continue;
                };
                for (name, value) in entries.iter() {
                    let key = (category, name.clone());
                    if !uses.contains(&key) || !followed.insert(key) {
                        continue;
                    }
                    let Some(resolved) = self.resolved(value) else {
                        continue;
                    };
                    // Names in the resource's own dictionary or array can only stand for colour
                    // spaces: an image's, or the base of an indexed colour space.
                    let values: Vec<&Object> = match resolved {
                        Object::Dictionary(dict) => dict.iter().map(|(_, v)| v).collect(),
                        Object::Stream(stream) => stream.dict.iter().map(|(_, v)| v).collect(),
                        Object::Array(items) => items.iter().collect(),
                        _ => Vec::new(),
                    };
                    for value in values {
                        let items = match value {
                            Object::Array(items) => items.iter().collect(),
                            other => vec![other],
                        };
                        more.extend(
                            items
                                .into_iter()
                                .filter_map(|v| v.as_name().ok())
                                .map(|n| (COLOR_SPACE, n.to_vec())),
                        );
                    }
                    if category == EXT_G_STATE {
                        if let Some(id) = self.soft_mask_form(value) {
                            if let Ok(Object::Stream(mask)) = self.doc.get_object(id) {
                                if !self.has_own_resources(&mask.dict) && is_content(&mask.dict) {
                                    let Some(found) = scan.uses_in(Some(id), mask)? else {
                                        return Ok(None);
                                    };
                                    more.extend(found);
                                }
                            }
                        }
                    }
                    match resolved {
                        Object::Stream(stream)
                            if !self.has_own_resources(&stream.dict)
                                && is_content(&stream.dict) =>
                        {
                            let Some(found) = scan.uses_in(value.as_reference().ok(), stream)?
                            else {
                                return Ok(None);
                            };
                            more.extend(found);
                        }
                        Object::Dictionary(font)
                            if font
                                .get(b"Subtype")
                                .and_then(Object::as_name)
                                .is_ok_and(|s| s == b"Type3")
                                && !self.has_own_resources(font) =>
                        {
                            let Object::Dictionary(procs) = font
                                .get(b"CharProcs")
                                .map(|o| self.resolve(o))
                                .unwrap_or(Object::Null)
                            else {
                                continue;
                            };
                            for (_, glyph) in procs.iter() {
                                if let Some(Object::Stream(stream)) = self.resolved(glyph) {
                                    let Some(found) =
                                        scan.uses_in(glyph.as_reference().ok(), stream)?
                                    else {
                                        return Ok(None);
                                    };
                                    more.extend(found);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            let before = uses.len();
            uses.extend(more);
            if uses.len() == before {
                return Ok(Some(uses));
            }
        }
    }

    /// `resources` with only the named entries in `uses` and the default colour spaces, as a
    /// direct dictionary.
    fn keep_named(&self, resources: &Dictionary, uses: &Uses) -> Dictionary {
        let mut kept = Dictionary::new();
        for (key, value) in resources.iter() {
            let Some(category) = NAMED_RESOURCES.into_iter().find(|c| *c == key.as_slice()) else {
                kept.set(key.clone(), value.clone());
                continue;
            };
            let Object::Dictionary(entries) = self.resolve(value) else {
                kept.set(key.clone(), value.clone());
                continue;
            };
            let used: Dictionary = entries
                .iter()
                .filter(|(name, _)| {
                    uses.contains(&(category, name.to_vec()))
                        || (category == COLOR_SPACE && DEFAULT_SPACES.contains(&name.as_slice()))
                })
                .map(|(n, v)| (n.clone(), v.clone()))
                .collect();
            if !used.is_empty() {
                kept.set(key.clone(), used);
            }
        }
        kept
    }

    /// Trims the resources of each form, tiling pattern and Type 3 font in `resources` that has
    /// resources of its own to what its own content uses, and so on down. The resources of objects
    /// that change go into `changed`, by object; entries written straight into `resources` change
    /// in place. An object is trimmed once however often it is drawn, so shared and looping
    /// resources end too.
    fn trim_nested(
        &self,
        resources: &mut Dictionary,
        scan: &mut NameScan,
        changed: &mut HashMap<ObjectId, Dictionary>,
        depth: usize,
    ) -> Result<()> {
        for category in [XOBJECT, PATTERN, FONT] {
            let Ok(value) = resources.get(category) else {
                continue;
            };
            let Object::Dictionary(mut entries) = self.resolve(value) else {
                continue;
            };
            let mut edited = false;
            for (_, entry) in entries.iter_mut() {
                if let Object::Reference(id) = entry {
                    self.trim_referenced(*id, scan, changed, depth)?;
                } else if let Some(kept) =
                    self.trim_own_resources(None, entry, scan, changed, depth)?
                {
                    match entry {
                        Object::Stream(stream) => stream.dict.set("Resources", kept),
                        Object::Dictionary(dict) => dict.set("Resources", kept),
                        _ => {}
