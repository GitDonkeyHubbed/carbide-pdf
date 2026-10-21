//! Removing what removed pages leave behind.
//!
//! Deleting a page, redacting it or leaving it out of an extract takes it out of the page tree,
//! but other parts of the document can still refer to it or to things only it used: form fields
//! hold the values of its widgets, structure elements hold its tagged text, links point at it,
//! and a resource dictionary shared between pages keeps the images and forms only it drew.
//! Anything still referred to is kept when the document is written, so these are cleaned up too.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::content::{name, tokens, Token};
use crate::error::{Error, Result};
use crate::limits::{messages, DecodeBudget, MAX_FORM_DEPTH};
use crate::pdf::Pdf;

const FONT: &[u8] = b"Font";
const XOBJECT: &[u8] = b"XObject";
const EXT_G_STATE: &[u8] = b"ExtGState";
const COLOR_SPACE: &[u8] = b"ColorSpace";
const PATTERN: &[u8] = b"Pattern";
const SHADING: &[u8] = b"Shading";
const PROPERTIES: &[u8] = b"Properties";
