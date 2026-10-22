                    }
                    edited = true;
                }
            }
            if edited {
                resources.set(category, entries);
            }
        }
        // A soft mask's form is drawn through the graphics state that names it.
        if let Some(Object::Dictionary(states)) = resources
            .get(EXT_G_STATE)
            .ok()
            .and_then(|s| self.resolved(s))
        {
            for (_, state) in states.iter() {
                if let Some(id) = self.soft_mask_form(state) {
                    self.trim_referenced(id, scan, changed, depth)?;
                }
            }
        }
        Ok(())
    }

    /// [`Self::trim_own_resources`] for the object `id`, once however often it is reached.
    fn trim_referenced(
        &self,
        id: ObjectId,
        scan: &mut NameScan,
        changed: &mut HashMap<ObjectId, Dictionary>,
        depth: usize,
    ) -> Result<()> {
        if !scan.trimmed.insert(id) {
            return Ok(());
        }
        let Ok(object) = self.doc.get_object(id) else {
            return Ok(());
        };
        if let Some(kept) = self.trim_own_resources(Some(id), object, scan, changed, depth)? {
            changed.insert(id, kept);
        }
        Ok(())
    }

    /// The form a graphics state's soft mask (`/SMask` `/G`) draws, if it has one.
    fn soft_mask_form(&self, state: &Object) -> Option<ObjectId> {
        let Some(Object::Dictionary(state)) = self.resolved(state) else {
            return None;
        };
        let Some(Object::Dictionary(mask)) =
            state.get(b"SMask").ok().and_then(|m| self.resolved(m))
        else {
            return None;
        };
        mask.get(b"G").and_then(Object::as_reference).ok()
    }

    /// The appearances of the annotations on `page`: normal, rollover and down, one for each state
    /// of the annotation where it has several.
    fn appearances_on(&self, page: ObjectId) -> Vec<ObjectId> {
        let mut found = Vec::new();
        let Some(Object::Array(annotations)) = self
            .doc
            .get_dictionary(page)
            .ok()
            .and_then(|p| p.get(b"Annots").ok())
            .and_then(|a| self.resolved(a))
        else {
            return found;
        };
        for annotation in annotations {
            let Some(Object::Dictionary(annotation)) = self.resolved(annotation) else {
                continue;
            };
            let Some(Object::Dictionary(appearances)) =
                annotation.get(b"AP").ok().and_then(|ap| self.resolved(ap))
            else {
                continue;
            };
            for key in [&b"N"[..], b"R", b"D"] {
                let Ok(appearance) = appearances.get(key) else {
                    continue;
                };
                match self.resolved(appearance) {
                    Some(Object::Dictionary(states)) => found.extend(
                        states
                            .iter()
                            .filter_map(|(_, state)| state.as_reference().ok()),
                    ),
                    _ => found.extend(appearance.as_reference()),
                }
            }
        }
        found
    }

    /// Trims the resources of the appearances of the annotations on `page` as
    /// [`Self::trim_nested`] trims those of the forms the page draws. An annotation that stays
    /// keeps its appearances, and their resources can be shared with content that went.
    fn trim_appearances(
        &self,
        page: ObjectId,
        scan: &mut NameScan,
        changed: &mut HashMap<ObjectId, Dictionary>,
    ) -> Result<()> {
        for id in self.appearances_on(page) {
            if !scan.trimmed.insert(id) {
                continue;
            }
            let Ok(object) = self.doc.get_object(id) else {
                continue;
            };
            if let Some(kept) = self.trim_own_resources(Some(id), object, scan, changed, 0)? {
                changed.insert(id, kept);
            }
        }
        Ok(())
    }

    /// Trims the resources `owner` (a form, a tiling pattern or a Type 3 font; object `id` when
    /// it is one of its own) has of its own to what its content uses, and those of what they hold
    /// in turn. The resources `owner` is left with, when they change. When its content cannot be
    /// read, its resources are kept whole, but what they hold is still trimmed. `depth` is how
    /// many such owners enclose it; one nested deeper than a page may draw forms (see
    /// [`MAX_FORM_DEPTH`]) is an error rather than left with resources that may be what removed
    /// content alone used.
    fn trim_own_resources(
        &self,
        id: Option<ObjectId>,
        owner: &Object,
        scan: &mut NameScan,
        changed: &mut HashMap<ObjectId, Dictionary>,
        depth: usize,
    ) -> Result<Option<Dictionary>> {
        let dict = match owner {
            Object::Stream(stream) => &stream.dict,
            Object::Dictionary(dict) => dict,
            _ => return Ok(None),
        };
        let Some(Object::Dictionary(resources)) =
            dict.get(b"Resources").ok().map(|r| self.resolve(r))
        else {
            return Ok(None);
        };
        let group = self.group_colour_space(dict);
        let content = match owner {
            Object::Stream(stream) if is_content(&stream.dict) => scan.uses_in(id, stream)?,
            Object::Dictionary(font)
                if font
                    .get(b"Subtype")
                    .and_then(Object::as_name)
                    .is_ok_and(|s| s == b"Type3") =>
            {
                self.glyph_uses(font, scan)?
            }
            _ => return Ok(None),
        };
        if depth >= MAX_FORM_DEPTH {
            return Err(Error::TooLarge(messages::TOO_COMPLEX.into()));
        }
        let uses = match content {
            Some(mut uses) => {
                uses.extend(group);
                self.scope_uses(uses, &resources, scan)?
            }
            None => None,
        };
        let mut kept = match &uses {
            Some(uses) => self.keep_named(&resources, uses),
            None => resources.clone(),
        };
        self.trim_nested(&mut kept, scan, changed, depth + 1)?;
        if kept == resources || kept == self.with_categories_resolved(&resources) {
            return Ok(None);
        }
        Ok(Some(kept))
    }

    /// The colour space a transparency group (`/Group` of a page or form) names from its resources
    /// as its blending space, which its content need not use.
    fn group_colour_space(&self, owner: &Dictionary) -> Option<(&'static [u8], Vec<u8>)> {
        let Object::Dictionary(group) = owner.get(b"Group").map(|g| self.resolve(g)).ok()? else {
            return None;
        };
        let name = group.get(b"CS").and_then(Object::as_name).ok()?;
        Some((COLOR_SPACE, name.to_vec()))
    }

    /// The resources the glyphs of the Type 3 font `font` use; `None` when one cannot be read.
    fn glyph_uses(&self, font: &Dictionary, scan: &mut NameScan) -> Result<Option<Uses>> {
        let mut uses = HashSet::new();
        let Object::Dictionary(procs) = font
            .get(b"CharProcs")
            .map(|o| self.resolve(o))
            .unwrap_or(Object::Null)
        else {
            return Ok(Some(uses));
        };
        for (_, glyph) in procs.iter() {
            let Some(Object::Stream(stream)) = self.resolved(glyph) else {
                continue;
