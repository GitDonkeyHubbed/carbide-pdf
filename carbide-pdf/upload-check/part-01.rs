    pub(crate) fn drop_structure_of(&mut self, pages: &HashSet<ObjectId>) {
        let Ok(catalog) = self.doc.catalog() else {
            return;
        };
        let Ok(root_id) = catalog
            .get(b"StructTreeRoot")
            .and_then(Object::as_reference)
        else {
            return;
        };
        let Ok(mut root) = self.doc.get_dictionary(root_id).cloned() else {
            return;
        };
        let annotations: HashSet<ObjectId> = self.annotations_on(pages).into_keys().collect();
        let mut removed = HashSet::new();
        if let Ok(mut kids) = root.get(b"K").cloned() {
            let scope = Scope {
                pages,
                annotations: &annotations,
            };
            if prune_structure(
                &mut self.doc,
                &mut kids,
                None,
                &scope,
                &mut removed,
                &mut HashSet::new(),
                0,
            ) {
                root.remove(b"K");
            } else {
                root.set("K", kids);
            }
        }
        if !removed.is_empty() {
            for (tree, key) in [(&b"ParentTree"[..], &b"Nums"[..]), (b"IDTree", b"Names")] {
                if let Ok(mut node) = root.get(tree).cloned() {
                    prune_tree(
                        &mut self.doc,
                        &mut node,
                        key,
                        &removed,
                        &mut HashSet::new(),
                        0,
                    );
                    root.set(tree, node);
                }
            }
        }
        self.doc.objects.insert(root_id, Object::Dictionary(root));
    }

    /// Removes every reference to `ids` from the document: array items and dictionary entries
    /// that point at them. Links to a dropped page lose their target, for instance.
    pub(crate) fn drop_references_to(&mut self, ids: &HashSet<ObjectId>) {
        for object in self.doc.objects.values_mut() {
            strip_references(object, ids);
        }
        strip_dict_references(&mut self.doc.trailer, ids);
    }

    /// Gives every page a resource dictionary of its own that holds only what its content uses,
    /// and stops page-tree nodes from passing resources down. Images, forms and fonts that only
    /// removed content used are then no longer part of the document, even when the pages shared
    /// one resource dictionary. A page whose content cannot be read keeps all of its resources.
    ///
    /// Each page's content, with the forms it draws, is decoded within the limit for one page in
    /// [`crate::limits`]; past it, with forms nested deeper than a page may draw them, or with
    /// more different resource names than [`MAX_NAMES`], this fails with [`Error::TooLarge`]
    /// rather than keep resources that may be what removed content alone used, and the document
    /// is left as it was.
    pub(crate) fn trim_resources(&mut self) -> Result<()> {
        let trim = self.plan_resource_trim(&self.page_ids())?;
        self.apply_resource_trim(trim);
        Ok(())
    }

    /// What [`Self::trim_resources`] changes for `pages`, the pages the document is to be left
    /// with, worked out without changing anything; it fails as that does. An edit that removes
    /// pages without changing the content or resources of those that stay can work this out
    /// first, while it can still refuse, and apply it once the pages are gone.
    pub(crate) fn plan_resource_trim(&self, pages: &[ObjectId]) -> Result<ResourceTrim> {
        let mut shared = HashSet::new();
        let mut seen = HashSet::new();
        for page in pages {
            if let Ok(id) = self
                .doc
                .get_dictionary(*page)
                .and_then(|d| d.get(b"Resources"))
                .and_then(Object::as_reference)
            {
                if !seen.insert(id) {
                    shared.insert(id);
                }
            }
        }
        let mut changes = Vec::new();
        let mut scan = NameScan {
            budget: DecodeBudget::for_page(),
            names_left: MAX_NAMES,
            names: HashMap::new(),
            joined: HashMap::new(),
            trimmed: HashSet::new(),
        };
        // Forms, patterns, Type 3 fonts and annotation appearances with resources of their own,
        // trimmed in turn.
        let mut nested = HashMap::new();
        for page in pages {
            let Some(Object::Dictionary(resources)) = self.inherited(*page, b"Resources") else {
                continue;
            };
            let own = self
                .doc
                .get_dictionary(*page)?
                .get(b"Resources")
                .ok()
                .cloned();
            scan.budget = DecodeBudget::for_page();
            scan.names_left = MAX_NAMES;
            let group = self.group_colour_space(self.doc.get_dictionary(*page)?);
            let mut kept = match self.names_used_by_page(*page, &resources, &mut scan)? {
                Some(mut uses) => {
                    uses.extend(group);
                    self.keep_named(&resources, &uses)
                }
                None => resources.clone(),
            };
            self.trim_nested(&mut kept, &mut scan, &mut nested, 0)?;
            self.trim_appearances(*page, &mut scan, &mut nested)?;
            let independent = match &own {
                Some(Object::Reference(id)) => !shared.contains(id),
                Some(_) => true,
                None => false,
            };
            let unchanged = kept == resources || kept == self.with_categories_resolved(&resources);
            if !(independent && unchanged) {
                changes.push((*page, kept));
            }
        }
        Ok(ResourceTrim {
            pages: changes,
            nested,
        })
    }

    /// Makes the changes [`Self::plan_resource_trim`] worked out. Only resource dictionaries are
    /// replaced, so nothing else is copied, and nothing here can fail. Pages that inherited
    /// resources already hold their own copy, so taking them off the nodes above does not change
    /// what those pages draw.
    pub(crate) fn apply_resource_trim(&mut self, trim: ResourceTrim) {
        for (id, resources) in trim.nested {
            match self.doc.get_object_mut(id) {
                Ok(Object::Stream(stream)) => stream.dict.set("Resources", resources),
                Ok(Object::Dictionary(dict)) => dict.set("Resources", resources),
                _ => {}
            }
        }
        for (page, resources) in trim.pages {
            if let Ok(dict) = self.doc.get_dictionary_mut(page) {
                dict.set("Resources", resources);
            }
        }
        let Ok(root) = self.pages_root() else {
            return;
        };
        let mut nodes = vec![root];
        let mut visited = HashSet::new();
        while let Some(node) = nodes.pop() {
            if !visited.insert(node) {
                continue;
            }
            // A sound tree can store /Kids as a reference to its array, and opening the file
            // leaves that reference in place. The array has to be followed, or every node below
            // this one keeps the resources a removed page drew.
            let kids = {
                let Ok(dict) = self.doc.get_dictionary_mut(node) else {
                    continue;
                };
                if !dict.has_type(b"Pages") {
                    continue;
                }
                dict.remove(b"Resources");
                dict.get(b"Kids").ok().cloned()
            };
            let Some(kids) = kids else {
                continue;
            };
            let Object::Array(kids) = self.resolve(&kids) else {
                continue;
            };
            nodes.extend(kids.iter().filter_map(|k| k.as_reference().ok()));
        }
    }

    /// The resources the page's content uses, with those the appearances of its annotations that
    /// have no resources of their own use (they are looked up in the page's), following forms,
    /// patterns and Type 3 fonts that use the page's resources instead of their own. `None` when
    /// some content cannot be read; an error when decoding it would go past the limits in
    /// [`crate::limits`].
    fn names_used_by_page(
