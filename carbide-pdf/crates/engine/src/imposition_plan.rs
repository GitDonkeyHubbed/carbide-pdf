//! Imposition planning: which source page lands in which cell of which printed side.
//!
//! This module is pure (no PDF objects), so every layout can be checked against the worked
//! examples recovered from the original feature.
//!
//! Terms: a *plate* is one printed side of one sheet and becomes one output page. Source pages are
//! numbered from 1 in the formulas (as in the original) and exposed as 0-based indexes in [`Cell`].

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    Booklet2Up,
    Booklet4Up,
    Booklet8Up,
    Sequential,
    StepAndRepeat,
    CutStacks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// Saddle-stitched: one nested booklet.
    Saddle,
    /// Perfect bound: independent folds ("signatures") of `sheets_per_fold` sheets.
    Perfect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Sheetwise,
    WorkAndTurn,
    WorkAndTumble,
}

/// Options tab, "Double sided printing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintingOrder {
    /// (front, back, front, back)
    Interleaved,
    /// Fronts first, then backs
    FrontsThenBacks,
    /// Fronts first, then backs reversed
    FrontsThenBacksReversed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanOptions {
    pub layout: LayoutKind,
    pub columns: usize,
    pub rows: usize,
    pub binding: Binding,
    pub sheets_per_fold: usize,
    pub style: Style,
    pub double_sided: bool,
    pub printing_order: PrintingOrder,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            layout: LayoutKind::Booklet2Up,
            columns: 2,
            rows: 1,
            binding: Binding::Saddle,
            sheets_per_fold: 1,
            style: Style::Sheetwise,
            double_sided: true,
            printing_order: PrintingOrder::Interleaved,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// 0-based source page, or `None` for a blank cell.
    pub page: Option<usize>,
    /// Rotated 180 degrees (top row of folded booklet grids).
    pub inverted: bool,
    /// Whole creep units this page moves toward the spine (0 at the spine, growing outward).
    pub creep_steps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plate {
    pub sheet: usize,
    pub front: bool,
    pub rows: usize,
    pub cols: usize,
    /// Row-major, row 0 at the top.
    pub cells: Vec<Cell>,
}

impl Plate {
    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.cells[row * self.cols + col]
    }

    /// The source pages in row-major order as 1-based numbers (0 = blank): handy in tests.
    pub fn numbers(&self) -> Vec<usize> {
        self.cells.iter().map(|c| c.page.map(|p| p + 1).unwrap_or(0)).collect()
    }
}

fn blank_if_over(number: usize, total: usize) -> Option<usize> {
    if number >= 1 && number <= total {
        Some(number - 1)
    } else {
        None
    }
}

fn round_up(value: usize, multiple: usize) -> usize {
    value.div_ceil(multiple) * multiple
}

/// Plates, in plate order (front/back interleaved), before the printing-order remap.
pub fn plan_plates(options: &PlanOptions, total_pages: usize) -> Result<Vec<Plate>> {
    if total_pages == 0 {
        return Err(Error::Other("the document has no pages".into()));
    }
    match options.layout {
        LayoutKind::Sequential => Ok(plan_sequential(options, total_pages)),
        LayoutKind::StepAndRepeat | LayoutKind::CutStacks => plan_tiled(options, total_pages),
        LayoutKind::Booklet2Up | LayoutKind::Booklet4Up | LayoutKind::Booklet8Up => plan_booklet(options, total_pages),
    }
}

// ---- simple layouts ------------------------------------------------------------------------

fn plan_sequential(options: &PlanOptions, total: usize) -> Vec<Plate> {
    let plates = total + usize::from(options.double_sided && total % 2 == 1);
    (0..plates)
        .map(|p| Plate {
            sheet: if options.double_sided { p / 2 } else { p },
            front: !options.double_sided || p % 2 == 0,
            rows: 1,
            cols: 1,
            cells: vec![Cell { page: blank_if_over(p + 1, total), inverted: false, creep_steps: 0 }],
        })
        .collect()
}

fn plan_tiled(options: &PlanOptions, total: usize) -> Result<Vec<Plate>> {
    let (cols, rows) = (options.columns, options.rows);
    if cols == 0 || rows == 0 {
        return Err(Error::Other("rows and columns must be at least 1".into()));
    }
    let cells = rows * cols;
    let d = options.double_sided;
    let plates = if d { 2 * total.div_ceil(2 * cells) } else { total.div_ceil(cells) };

    let mut out = Vec::with_capacity(plates);
    for p in 0..plates {
        let mut list = Vec::with_capacity(cells);
        for a in 0..rows {
            for b in 0..cols {
                let number = match options.layout {
                    LayoutKind::StepAndRepeat => {
                        if !d {
                            p * cells + a * cols + b + 1
                        } else if p % 2 == 0 {
                            p * cells + 2 * (a * cols + b) + 1
                        } else {
                            (p - 1) * cells + 2 * (a * cols + (cols - 1 - b)) + 2
                        }
                    }
                    // Cut stacks
                    _ => {
                        let column = if d && p % 2 == 1 { cols - 1 - b } else { b };
                        p + 1 + plates * (a * cols + column)
                    }
                };
                list.push(Cell { page: blank_if_over(number, total), inverted: false, creep_steps: 0 });
            }
        }
        out.push(Plate { sheet: if d { p / 2 } else { p }, front: !d || p % 2 == 0, rows, cols, cells: list });
    }
    Ok(out)
}

// ---- booklets ------------------------------------------------------------------------------

/// One block (the whole document, or one perfect-bound fold) laid out by a single routine.
///
/// `first` is the 1-based number of the block's first source page and `count` its length; any
/// computed page number past `first + count - 1` is blank. Plates are returned in push order.
struct Block {
    first: usize,
    last: usize,
}

impl Block {
    fn cell(&self, number: usize, inverted: bool) -> Cell {
        Cell {
            page: if number >= self.first && number <= self.last { Some(number - 1) } else { None },
            inverted,
            creep_steps: 0,
        }
    }

    fn plate(&self, sheet: usize, front: bool, rows: usize, cols: usize, cells: Vec<Cell>) -> Plate {
        Plate { sheet, front, rows, cols, cells }
    }
}

/// 2-up saddle: sheet `k` has front `[P-2k, 2k+1]` and back `[2k+2, P-2k-1]` (offset by `s`).
fn saddle_2up(b: &Block) -> Vec<Plate> {
    let s = b.first;
    let padded = round_up(b.last - b.first + 1, 4);
    let mut plates = Vec::new();
    for k in 0..padded / 4 {
        plates.push(b.plate(k, true, 1, 2, vec![b.cell(s + padded - 2 * k - 1, false), b.cell(s + 2 * k, false)]));
        plates.push(b.plate(k, false, 1, 2, vec![b.cell(s + 2 * k + 1, false), b.cell(s + padded - 2 * k - 2, false)]));
    }
    plates
}

/// 4-up saddle (2 columns x 2 rows), sheetwise. Top row inverted.
fn saddle_4up(b: &Block) -> Vec<Plate> {
    let s = b.first;
    let count = b.last - b.first + 1;
    let p = round_up(count, 8);
    let mut plates = Vec::new();
    for t in 0..p / 8 {
        // [TL, TR, BL, BR]
        plates.push(b.plate(
            t,
            true,
            2,
            2,
            vec![
                b.cell(s + p - 4 - 4 * t, true),
                b.cell(s + 4 * t + 3, true),
                b.cell(s + p - 1 - 4 * t, false),
                b.cell(s + 4 * t, false),
            ],
        ));
        plates.push(b.plate(
            t,
            false,
            2,
            2,
            vec![
                b.cell(s + 4 * t + 2, true),
                b.cell(s + p - 3 - 4 * t, true),
                b.cell(s + 4 * t + 1, false),
                b.cell(s + p - 2 - 4 * t, false),
            ],
        ));
    }
    plates
}

/// 4-up work and turn / tumble: self-contained plates, two per sheet.
fn work_and_turn_4up(b: &Block) -> Vec<Plate> {
    let s = b.first;
    let count = b.last - b.first + 1;
    let m = count.div_ceil(4);
    let q = s + 4 * m;
    (0..m)
        .map(|k| {
            b.plate(
                k / 2,
                true,
                2,
                2,
                vec![
                    b.cell(q - 2 * k - 2, true),
                    b.cell(s + 2 * k + 1, true),
                    b.cell(q - 2 * k - 1, false),
                    b.cell(s + 2 * k, false),
                ],
            )
        })
        .collect()
}

/// 8-up saddle (4 columns x 2 rows), sheetwise. Row 0 inverted.
fn saddle_8up(b: &Block) -> Vec<Plate> {
    let s = b.first;
    let count = b.last - b.first + 1;
    let p = round_up(count, 16);
    let mut plates = Vec::new();
    for t in 0..p / 16 {
        let row = |numbers: [usize; 4], inverted: bool| numbers.map(|n| b.cell(n, inverted));
        let front_top = row([s + p - 8 - 8 * t, s + 8 * t + 7, s + 8 * t + 4, s + p - 5 - 8 * t], true);
        let front_bottom = row([s + p - 1 - 8 * t, s + 8 * t, s + 8 * t + 3, s + p - 4 - 8 * t], false);
        let back_top = row([s + p - 6 - 8 * t, s + 8 * t + 5, s + 8 * t + 6, s + p - 7 - 8 * t], true);
        let back_bottom = row([s + p - 3 - 8 * t, s + 8 * t + 2, s + 8 * t + 1, s + p - 2 - 8 * t], false);
        plates.push(b.plate(t, true, 2, 4, front_top.into_iter().chain(front_bottom).collect()));
        plates.push(b.plate(t, false, 2, 4, back_top.into_iter().chain(back_bottom).collect()));
    }
    plates
}

/// 8-up work and turn (`tumble == false`) or work and tumble (`tumble == true`).
fn work_and_turn_8up(b: &Block, tumble: bool) -> Vec<Plate> {
    let s = b.first;
    let count = b.last - b.first + 1;
    let m = count.div_ceil(8);
    let p = 8 * m;
    (0..m)
        .map(|t| {
            let top = if tumble {
                [s + p - 2 - 4 * t, s + 4 * t + 1, s + 4 * t + 2, s + p - 3 - 4 * t]
            } else {
                [s + p - 4 - 4 * t, s + 4 * t + 3, s + 4 * t + 2, s + p - 3 - 4 * t]
            };
            let bottom = if tumble {
                [s + p - 1 - 4 * t, s + 4 * t, s + 4 * t + 3, s + p - 4 - 4 * t]
            } else {
                [s + p - 1 - 4 * t, s + 4 * t, s + 4 * t + 1, s + p - 2 - 4 * t]
            };
            let cells = top
                .into_iter()
                .map(|n| b.cell(n, true))
                .chain(bottom.into_iter().map(|n| b.cell(n, false)))
                .collect();
            b.plate(t / 2, true, 2, 4, cells)
        })
        .collect()
}

fn layout_block(options: &PlanOptions, block: &Block) -> Vec<Plate> {
    use LayoutKind::*;
    match (options.layout, options.style) {
        (Booklet2Up, _) => saddle_2up(block),
        (Booklet4Up, Style::Sheetwise) => saddle_4up(block),
        (Booklet4Up, _) => work_and_turn_4up(block),
        (Booklet8Up, Style::Sheetwise) => saddle_8up(block),
        (Booklet8Up, Style::WorkAndTurn) => work_and_turn_8up(block, false),
        (Booklet8Up, Style::WorkAndTumble) => work_and_turn_8up(block, true),
        _ => unreachable!("only booklet layouts reach here"),
    }
}

/// Creep steps for every cell of a block (arithmetic recovered from the original).
///
/// With `G` = spreads across, `R` = rows and `K` = sheets in the block (limited to the sheets per
/// fold when perfect bound), `U = G*R*K` (halved for work-and-turn/tumble). A page's distance from
/// the spine is `d`, and its creep is `floor(d/2)` units: zero at the spine, largest outside.
fn apply_creep(options: &PlanOptions, block: &Block, plates: &mut [Plate]) {
    let Some(first) = plates.first() else { return };
    let (cols, rows) = (first.cols, first.rows);
    let sheetwise = options.style == Style::Sheetwise || options.layout == LayoutKind::Booklet2Up;
    let sheets = if sheetwise { plates.len().div_ceil(2) } else { plates.len() };
    let k = match options.binding {
        Binding::Perfect => options.sheets_per_fold.max(1).min(sheets.max(1)),
        Binding::Saddle => sheets.max(1),
    };
    let mut u = (cols / 2).max(1) * rows * k;
    if !sheetwise {
        u = (u / 2).max(1);
    }
    let period = 4 * u;
    for plate in plates.iter_mut() {
        for cell in &mut plate.cells {
            if let Some(page) = cell.page {
                let q = (page + 1 - block.first) % period + 1; // 1..=4U
                let d = if q <= 2 * u { 2 * u - q } else { q - 2 * u - 1 };
                cell.creep_steps = (d / 2) as u32;
            }
        }
    }
}

fn plan_booklet(options: &PlanOptions, total: usize) -> Result<Vec<Plate>> {
    use LayoutKind::*;
    let k = options.sheets_per_fold.max(1);

    match options.binding {
        Binding::Saddle => {
            let block = Block { first: 1, last: total };
            let mut plates = layout_block(options, &block);
            apply_creep(options, &block, &mut plates);
            Ok(plates)
        }
        Binding::Perfect => {
            // Folds ("signatures") of whole sheets; each is imposed by its own routine and padded
            // on its own.
            let fold_pages = match (options.layout, options.style) {
                (Booklet2Up, _) => 4 * k,
                (Booklet4Up, Style::Sheetwise) => 8 * k,
                (Booklet4Up, _) => 4 * k,
                (Booklet8Up, Style::Sheetwise) => 16 * k,
                (Booklet8Up, _) => 8 * k,
                _ => unreachable!(),
            };
            let mut plates: Vec<Plate> = Vec::new();
            let mut first = 1;
            while first <= total {
                let last = (first + fold_pages - 1).min(total);
                let block = Block { first, last };
                let mut fold = layout_block(options, &block);
                apply_creep(options, &block, &mut fold);
                let base_sheet = plates.last().map(|p| p.sheet + 1).unwrap_or(0);
                for plate in &mut fold {
                    plate.sheet += base_sheet;
                }
                plates.extend(fold);
                first += fold_pages;
            }
            Ok(plates)
        }
    }
}

// ---- printing order ------------------------------------------------------------------------

/// For each output page `i`, the plate that is printed there.
///
/// The remap only applies for double-sided output and when the style is sheetwise or the layout
/// is perfect bound (`sheets_per_fold_active`), exactly as in the original.
pub fn output_order(
    plate_count: usize,
    mode: PrintingOrder,
    double_sided: bool,
    style: Style,
    sheets_per_fold_active: bool,
) -> Vec<usize> {
    let identity = || (0..plate_count).collect::<Vec<_>>();
    let applies = mode != PrintingOrder::Interleaved
        && double_sided
        && (style == Style::Sheetwise || sheets_per_fold_active);
    if !applies {
        return identity();
    }
    let half = plate_count / 2;
    (0..plate_count)
        .map(|i| {
            if i < half {
                2 * i
            } else {
                let back = 2 * (i - half) + 1;
                match mode {
                    PrintingOrder::FrontsThenBacksReversed => plate_count - back,
                    _ => back,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn booklet(layout: LayoutKind, binding: Binding, style: Style, pages: usize) -> Vec<Vec<usize>> {
        let options = PlanOptions {
            layout,
            binding,
            style,
            columns: if layout == LayoutKind::Booklet2Up { 2 } else { 2 },
            rows: if layout == LayoutKind::Booklet2Up { 1 } else { 2 },
            ..Default::default()
        };
        plan_plates(&options, pages).unwrap().iter().map(Plate::numbers).collect()
    }

    #[test]
    fn example_a_eight_pages_saddle_two_up() {
        assert_eq!(
            booklet(LayoutKind::Booklet2Up, Binding::Saddle, Style::Sheetwise, 8),
            vec![vec![8, 1], vec![2, 7], vec![6, 3], vec![4, 5]]
        );
    }

    #[test]
    fn example_b_six_pages_pads_with_blanks() {
        assert_eq!(
            booklet(LayoutKind::Booklet2Up, Binding::Saddle, Style::Sheetwise, 6),
            vec![vec![0, 1], vec![2, 0], vec![6, 3], vec![4, 5]]
        );
    }

    #[test]
    fn example_c_eight_pages_saddle_four_up() {
        let plates = plan_plates(
            &PlanOptions { layout: LayoutKind::Booklet4Up, columns: 2, rows: 2, ..Default::default() },
            8,
        )
        .unwrap();
        assert_eq!(plates.len(), 2);
        assert_eq!(plates[0].numbers(), vec![5, 4, 8, 1]);
        assert_eq!(plates[1].numbers(), vec![3, 6, 2, 7]);
        // Top row is inverted, bottom row is not.
        assert!(plates[0].cell(0, 0).inverted && plates[0].cell(0, 1).inverted);
        assert!(!plates[0].cell(1, 0).inverted);
    }

    #[test]
    fn example_c_six_page_variant() {
        let plates = plan_plates(
            &PlanOptions { layout: LayoutKind::Booklet4Up, columns: 2, rows: 2, ..Default::default() },
            6,
        )
        .unwrap();
        assert_eq!(plates[0].numbers(), vec![5, 4, 0, 1]);
        assert_eq!(plates[1].numbers(), vec![3, 6, 2, 0]);
    }

    #[test]
    fn example_d_perfect_bound_two_up() {
        assert_eq!(
            booklet(LayoutKind::Booklet2Up, Binding::Perfect, Style::Sheetwise, 8),
            vec![vec![4, 1], vec![2, 3], vec![8, 5], vec![6, 7]]
        );
    }

    #[test]
    fn example_e_work_and_turn_four_up() {
        for style in [Style::WorkAndTurn, Style::WorkAndTumble] {
            let plates = plan_plates(
                &PlanOptions { layout: LayoutKind::Booklet4Up, style, columns: 2, rows: 2, ..Default::default() },
                8,
            )
            .unwrap();
            assert_eq!(plates.len(), 2);
            assert_eq!(plates[0].numbers(), vec![7, 2, 8, 1]);
            assert_eq!(plates[1].numbers(), vec![5, 4, 6, 3]);
            assert!(plates.iter().all(|p| p.front));
        }
    }

    #[test]
    fn every_source_page_is_placed_exactly_once() {
        for pages in 1..=70 {
            for layout in [LayoutKind::Booklet2Up, LayoutKind::Booklet4Up, LayoutKind::Booklet8Up] {
                for binding in [Binding::Saddle, Binding::Perfect] {
                    for style in [Style::Sheetwise, Style::WorkAndTurn, Style::WorkAndTumble] {
                        let options = PlanOptions {
                            layout,
                            binding,
                            style,
                            sheets_per_fold: 2,
                            columns: if layout == LayoutKind::Booklet8Up { 4 } else { 2 },
                            rows: if layout == LayoutKind::Booklet2Up { 1 } else { 2 },
                            ..Default::default()
                        };
                        let plates = plan_plates(&options, pages).unwrap();
                        let mut seen: Vec<usize> =
                            plates.iter().flat_map(|p| p.cells.iter().filter_map(|c| c.page)).collect();
                        seen.sort_unstable();
                        assert_eq!(seen, (0..pages).collect::<Vec<_>>(), "{layout:?} {binding:?} {style:?} N={pages}");
                    }
                }
            }
        }
    }

    #[test]
    fn simple_layouts_follow_the_recovered_formulas() {
        // Step and repeat, 2 x 2, single sided: contiguous blocks, row-major.
        let step = PlanOptions {
            layout: LayoutKind::StepAndRepeat,
            columns: 2,
            rows: 2,
            double_sided: false,
            ..Default::default()
        };
        let plates = plan_plates(&step, 6).unwrap();
        assert_eq!(plates.iter().map(Plate::numbers).collect::<Vec<_>>(), vec![vec![1, 2, 3, 4], vec![5, 6, 0, 0]]);

        // Double sided: even pages on the back with columns mirrored.
        let duplex = PlanOptions { double_sided: true, ..step };
        let plates = plan_plates(&duplex, 8).unwrap();
        assert_eq!(plates[0].numbers(), vec![1, 3, 5, 7]);
        assert_eq!(plates[1].numbers(), vec![4, 2, 8, 6]);
        assert!(plates[0].front && !plates[1].front);

        // Cut stacks: each plate is a stack, so page p+1 repeats every `plates` pages.
        let cut = PlanOptions { layout: LayoutKind::CutStacks, double_sided: false, ..step };
        let plates = plan_plates(&cut, 8).unwrap();
        assert_eq!(plates.len(), 2);
        assert_eq!(plates[0].numbers(), vec![1, 3, 5, 7]);
        assert_eq!(plates[1].numbers(), vec![2, 4, 6, 8]);

        // Sequential: one page per plate, padded to an even count when double sided.
        let seq = PlanOptions { layout: LayoutKind::Sequential, double_sided: true, ..step };
        assert_eq!(plan_plates(&seq, 5).unwrap().len(), 6);
    }

    #[test]
    fn printing_order_matches_the_recovered_examples() {
        let order = |mode| output_order(4, mode, true, Style::Sheetwise, false);
        assert_eq!(order(PrintingOrder::Interleaved), vec![0, 1, 2, 3]);
        assert_eq!(order(PrintingOrder::FrontsThenBacks), vec![0, 2, 1, 3]);
        assert_eq!(order(PrintingOrder::FrontsThenBacksReversed), vec![0, 2, 3, 1]);
        // Guard: work and turn without perfect folds, or single-sided output, keeps the order.
        assert_eq!(
            output_order(4, PrintingOrder::FrontsThenBacks, true, Style::WorkAndTurn, false),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            output_order(4, PrintingOrder::FrontsThenBacks, false, Style::Sheetwise, false),
            vec![0, 1, 2, 3]
        );
    }

    fn plan(layout: LayoutKind, binding: Binding, style: Style, k: usize, pages: usize) -> Vec<Plate> {
        let (columns, rows) = match layout {
            LayoutKind::Booklet2Up => (2, 1),
            LayoutKind::Booklet4Up => (2, 2),
            _ => (4, 2),
        };
        plan_plates(
            &PlanOptions { layout, binding, style, sheets_per_fold: k, columns, rows, ..Default::default() },
            pages,
        )
        .unwrap()
    }

    #[test]
    fn four_up_saddle_larger_page_counts() {
        let p16 = plan(LayoutKind::Booklet4Up, Binding::Saddle, Style::Sheetwise, 1, 16);
        assert_eq!(
            p16.iter().map(Plate::numbers).collect::<Vec<_>>(),
            vec![vec![13, 4, 16, 1], vec![3, 14, 2, 15], vec![9, 8, 12, 5], vec![7, 10, 6, 11]]
        );
        let p24 = plan(LayoutKind::Booklet4Up, Binding::Saddle, Style::Sheetwise, 1, 24);
        assert_eq!(
            p24.iter().map(Plate::numbers).collect::<Vec<_>>(),
            vec![
                vec![21, 4, 24, 1],
                vec![3, 22, 2, 23],
                vec![17, 8, 20, 5],
                vec![7, 18, 6, 19],
                vec![13, 12, 16, 9],
                vec![11, 14, 10, 15],
            ]
        );
        // Two plates per sheet.
        assert_eq!(p24.iter().map(|p| p.sheet).collect::<Vec<_>>(), vec![0, 0, 1, 1, 2, 2]);
    }

    #[test]
    fn four_up_work_and_turn_larger_page_counts() {
        let plates = plan(LayoutKind::Booklet4Up, Binding::Saddle, Style::WorkAndTurn, 1, 16);
        assert_eq!(
            plates.iter().map(Plate::numbers).collect::<Vec<_>>(),
            vec![vec![15, 2, 16, 1], vec![13, 4, 14, 3], vec![11, 6, 12, 5], vec![9, 8, 10, 7]]
        );
        // Plates 2j and 2j+1 are the two faces of sheet j.
        assert_eq!(plates.iter().map(|p| p.sheet).collect::<Vec<_>>(), vec![0, 0, 1, 1]);
    }

    #[test]
    fn eight_up_saddle_examples() {
        let full = plan(LayoutKind::Booklet8Up, Binding::Saddle, Style::Sheetwise, 1, 16);
        assert_eq!(full.len(), 2);
        assert_eq!(full[0].numbers(), vec![9, 8, 5, 12, 16, 1, 4, 13]);
        assert_eq!(full[1].numbers(), vec![11, 6, 7, 10, 14, 3, 2, 15]);
        // Row 0 inverted, row 1 upright.
        assert!((0..4).all(|c| full[0].cell(0, c).inverted && !full[0].cell(1, c).inverted));

        let twelve = plan(LayoutKind::Booklet8Up, Binding::Saddle, Style::Sheetwise, 1, 12);
        assert_eq!(twelve[0].numbers(), vec![9, 8, 5, 12, 0, 1, 4, 0]);
        assert_eq!(twelve[1].numbers(), vec![11, 6, 7, 10, 0, 3, 2, 0]);
    }

    #[test]
    fn eight_up_work_and_turn_and_tumble_examples() {
        let turn = plan(LayoutKind::Booklet8Up, Binding::Saddle, Style::WorkAndTurn, 1, 16);
        assert_eq!(turn[0].numbers(), vec![13, 4, 3, 14, 16, 1, 2, 15]);
        assert_eq!(turn[1].numbers(), vec![9, 8, 7, 10, 12, 5, 6, 11]);
        let tumble = plan(LayoutKind::Booklet8Up, Binding::Saddle, Style::WorkAndTumble, 1, 16);
        assert_eq!(tumble[0].numbers(), vec![15, 2, 3, 14, 16, 1, 4, 13]);
        assert_eq!(tumble[1].numbers(), vec![11, 6, 7, 10, 12, 5, 8, 9]);
        let short = plan(LayoutKind::Booklet8Up, Binding::Saddle, Style::WorkAndTurn, 1, 12);
        assert_eq!(short[0].numbers(), vec![0, 4, 3, 0, 0, 1, 2, 0]);
    }

    #[test]
    fn perfect_bound_folds_and_short_last_fold() {
        // 8-up perfect bound, one sheet per fold (16 pages per fold), 20 pages: the last fold has
        // pages 17..20 in the two middle columns of row 1.
        let plates = plan(LayoutKind::Booklet8Up, Binding::Perfect, Style::Sheetwise, 1, 20);
        assert_eq!(plates.len(), 4);
        assert_eq!(plates[2].numbers(), vec![0, 0, 0, 0, 0, 17, 20, 0]);
        assert_eq!(plates[3].numbers(), vec![0, 0, 0, 0, 0, 19, 18, 0]);

        // 2-up perfect bound, 6 pages: fold 1 holds pages 5 and 6 only.
        let two_up = plan(LayoutKind::Booklet2Up, Binding::Perfect, Style::Sheetwise, 1, 6);
        assert_eq!(
            two_up.iter().map(Plate::numbers).collect::<Vec<_>>(),
            vec![vec![4, 1], vec![2, 3], vec![0, 5], vec![6, 0]]
        );
    }

    #[test]
    fn creep_is_zero_at_the_spine_and_grows_outward() {
        // 16-page 2-up saddle: 4 sheets, U = 4, block of 16 pages.
        let plates = plan(LayoutKind::Booklet2Up, Binding::Saddle, Style::Sheetwise, 1, 16);
        let steps: Vec<(usize, u32)> = plates
            .iter()
            .flat_map(|p| p.cells.iter())
            .filter_map(|c| c.page.map(|page| (page + 1, c.creep_steps)))
            .collect();
        let of = |page: usize| steps.iter().find(|(p, _)| *p == page).unwrap().1;
        assert_eq!(of(1), 3, "outermost pages move the most");
        assert_eq!(of(16), 3);
        assert_eq!(of(8), 0, "pages at the spine do not move");
        assert_eq!(of(9), 0);
        assert!(of(1) >= of(3) && of(3) >= of(5) && of(5) >= of(7));
    }
}
