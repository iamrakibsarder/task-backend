//! Invoice PDFs, written directly (PDF 1.4, the built-in Helvetica fonts, no dependencies).
//!
//! The layout follows the app's invoice file (`taskx-frontend/src/domain/invoiceExport.ts`):
//! sender, invoice number, billed-to, dates, line items, totals, how to pay, notes.
//! Long invoices flow onto more pages; every page has a "page x of y" footer.
//!
//! Text uses WinAnsi encoding, which covers English and Western European text and € £ ¥.
//! Characters outside it are printed as `?`.

use chrono::{Datelike, NaiveDate};
use rust_decimal::prelude::ToPrimitive;

use crate::models::{Invoice, SenderSnapshot};

/* ---------------- Fonts & text ---------------- */

#[derive(Clone, Copy, PartialEq)]
enum Font {
    Regular,
    Bold,
}

/// Helvetica advance widths (1/1000 em) for ASCII 32..=126, from the standard AFM.
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278,
    278, // space../
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // 0..9
    278, 278, 584, 584, 584, 556, 1015, // :..@
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667,
    611, 722, 667, 944, 667, 667, 611, // A..Z
    278, 278, 278, 469, 556, 333, // [..`
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500,
    278, 556, 500, 722, 500, 500, 500, // a..z
    334, 260, 334, 584, // {..~
];

/// One character in WinAnsi, or `?`.
fn win_ansi(c: char) -> u8 {
    match c {
        ' '..='~' => c as u8,
        '\u{a0}'..='\u{ff}' => c as u32 as u8,
        '€' => 0x80,
        '…' => 0x85,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '−' => b'-',
        '\t' => b' ',
        _ => b'?',
    }
}

/// Widths for WinAnsi 128..=255 (Helvetica AFM; accented letters as their base letter).
fn high_width(b: u8) -> u16 {
    match b {
        0x80 => 600,         // € (a little extra room; see `Doc::text`)
        0x85 | 0x97 => 1000, // … —
        0x91 | 0x92 => 222,  // ‘ ’
        0x93 | 0x94 => 333,  // “ ”
        0x95 => 350,         // •
        0xA0 => 278,
        0xA1 | 0xA8 | 0xAD | 0xAF | 0xB2 | 0xB3 | 0xB4 | 0xB8 | 0xB9 => 333,
        0xA6 => 260,
        0xA9 | 0xAE => 737,
        0xAA => 370,
        0xAC | 0xB1 | 0xD7 | 0xF7 => 584,
        0xB0 => 400,
        0xB6 => 537,
        0xB7 => 278,
        0xBA => 365,
        0xBC..=0xBE => 834,
        0xBF => 611,
        0xC0..=0xC5 | 0xC8..=0xCB | 0xDD | 0xDE => 667,
        0xC6 => 1000,
        0xC7 | 0xD0 | 0xD1 | 0xD9..=0xDC => 722,
        0xCC..=0xCF | 0xEC..=0xEF => 278,
        0xD2..=0xD6 | 0xD8 => 778,
        0xDF | 0xF8 => 611,
        0xE6 => 889,
        0xE7 | 0xFD | 0xFF => 500,
        _ => 556,
    }
}

/// Advance width in 1/1000 em. The same numbers go into the PDF's `/Widths`, so what we
/// measure for alignment is exactly what viewers draw (and the euro sign has a width:
/// the original Helvetica metrics predate it).
fn glyph_width(b: u8, font: Font) -> u16 {
    let w = match b {
        32..=126 => HELVETICA[(b - 32) as usize],
        127..=255 => high_width(b),
        _ => 0,
    };
    // Bold letters run a little wider; digits and punctuation keep their width.
    let letter = b.is_ascii_alphabetic() || ((0xC0..=0xFF).contains(&b) && b != 0xD7 && b != 0xF7);
    if font == Font::Bold && letter {
        (w as f32 * 1.07).round() as u16
    } else {
        w
    }
}

fn char_width(b: u8, font: Font) -> f32 {
    glyph_width(b, font) as f32
}

fn text_width(s: &str, size: f32, font: Font) -> f32 {
    s.chars()
        .map(|c| char_width(win_ansi(c), font))
        .sum::<f32>()
        * size
        / 1000.0
}

/// Splits `s` into lines no wider than `max` points, breaking at spaces (or mid-word if
/// a single word is too long). Keeps the line breaks already in `s`.
fn wrap(s: &str, size: f32, font: Font, max: f32) -> Vec<String> {
    let mut out = Vec::new();
    for para in s.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if text_width(&candidate, size, font) <= max {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            // A word longer than the line: cut it.
            let mut piece = String::new();
            for ch in word.chars() {
                piece.push(ch);
                if text_width(&piece, size, font) > max {
                    piece.pop();
                    out.push(std::mem::take(&mut piece));
                    piece.push(ch);
                }
            }
            line = piece;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/* ---------------- Formatting (matches the app) ---------------- */

const MON3: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn long_date(d: NaiveDate) -> String {
    format!("{} {} {}", d.day(), MON3[d.month0() as usize], d.year())
}

fn month_label(m: &str) -> String {
    let mut it = m.split('-');
    match (
        it.next().and_then(|y| y.parse::<i32>().ok()),
        it.next().and_then(|m| m.parse::<usize>().ok()),
    ) {
        (Some(y), Some(mm)) if (1..=12).contains(&mm) => format!("{} {y}", MONTHS[mm - 1]),
        _ => m.to_string(),
    }
}

fn hours(min: i64) -> String {
    format!("{}:{:02}", min / 60, min % 60)
}

/// `$1,234.50`, `€80.00`, `¥12,000`, or `BDT 1,000.00` for currencies without a WinAnsi symbol.
pub fn money(n: f64, currency: &str) -> String {
    let (prefix, decimals) = match currency {
        "USD" => ("$", 2),
        "EUR" => ("€", 2),
        "GBP" => ("£", 2),
        "JPY" => ("¥", 0),
        "CAD" => ("CA$", 2),
        "AUD" => ("A$", 2),
        "NZD" => ("NZ$", 2),
        other => return format!("{other} {}", grouped(n, 2)),
    };
    let sign = if n < -0.0049 { "-" } else { "" };
    format!("{sign}{prefix}{}", grouped(n.abs(), decimals))
}

fn grouped(n: f64, decimals: usize) -> String {
    let s = format!("{:.*}", decimals, n.abs());
    let (int, frac) = s
        .split_once('.')
        .map_or((s.as_str(), None), |(a, b)| (a, Some(b)));
    let mut out = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    if n < -0.0049 {
        format!("-{out}")
    } else {
        out
    }
}

/// Subtotal → discount → tax → total, then credits and payments → balance (as in the app).
pub struct Totals {
    pub subtotal: f64,
    pub discount: f64,
    pub tax: f64,
    pub total: f64,
    pub credited: f64,
    pub paid: f64,
    pub balance: f64,
}

pub fn totals(inv: &Invoice) -> Totals {
    let r2 = |x: f64| (x * 100.0).round() / 100.0;
    let subtotal = r2(inv.lines.iter().map(|l| l.amount).sum());
    let discount = r2(inv.discount.to_f64().unwrap_or(0.0));
    let taxable = (subtotal - discount).max(0.0);
    let tax = r2(taxable * inv.tax_rate.to_f64().unwrap_or(0.0) / 100.0);
    let total = r2(taxable + tax);
    let credited = r2(inv.credits.iter().map(|c| c.amount).sum());
    let paid = r2(inv.payments.iter().map(|p| p.amount).sum());
    Totals {
        subtotal,
        discount,
        tax,
        total,
        credited,
        paid,
        balance: r2((total - credited - paid).max(0.0)),
    }
}

/* ---------------- Page drawing ---------------- */

const W: f32 = 595.28; // A4
const H: f32 = 841.89;
const LEFT: f32 = 50.0;
const RIGHT: f32 = W - 50.0;
const TOP: f32 = H - 56.0;
const BOTTOM: f32 = 64.0;

const INK: (f32, f32, f32) = (0.047, 0.008, 0.102);
const MUTED: (f32, f32, f32) = (0.36, 0.36, 0.39);
const LINE: (f32, f32, f32) = (0.92, 0.91, 0.945);
const PANEL: (f32, f32, f32) = (0.96, 0.953, 0.976);

struct Doc {
    pages: Vec<String>,
    y: f32,
}

impl Doc {
    fn new() -> Self {
        Self {
            pages: vec![String::new()],
            y: TOP,
        }
    }
    fn ops(&mut self) -> &mut String {
        self.pages.last_mut().expect("a page")
    }
    fn new_page(&mut self) {
        self.pages.push(String::new());
        self.y = TOP;
    }
    /// Starts a new page unless `need` points still fit.
    fn room(&mut self, need: f32) -> bool {
        if self.y - need < BOTTOM {
            self.new_page();
            true
        } else {
            false
        }
    }
    fn text(&mut self, x: f32, y: f32, size: f32, font: Font, color: (f32, f32, f32), s: &str) {
        if s.is_empty() {
            return;
        }
        let f = if font == Font::Bold { "F2" } else { "F1" };
        let (r, g, b) = color;
        // Some viewers (macOS) draw the euro sign wider than its advance in the built-in
        // Helvetica, so each € is placed on its own at the position we measured, slightly
        // condensed so it fits its slot whichever font the viewer substitutes.
        let mut op = format!("BT {r:.3} {g:.3} {b:.3} rg /{f} {size} Tf ");
        let mut cx = x;
        let mut run = String::new();
        let flush = |op: &mut String, run: &mut String, at: f32| {
            if !run.is_empty() {
                let hex: String = run
                    .chars()
                    .map(|c| format!("{:02X}", win_ansi(c)))
                    .collect();
                op.push_str(&format!("1 0 0 1 {at:.2} {y:.2} Tm <{hex}> Tj "));
                run.clear();
            }
        };
        let mut start = cx;
        for ch in s.chars() {
            if ch == '€' {
                flush(&mut op, &mut run, start);
                op.push_str(&format!("0.85 0 0 1 {cx:.2} {y:.2} Tm <80> Tj "));
                cx += char_width(0x80, font) * size / 1000.0;
                start = cx;
            } else {
                run.push(ch);
                cx += char_width(win_ansi(ch), font) * size / 1000.0;
            }
        }
        flush(&mut op, &mut run, start);
        op.push_str("ET\n");
        self.ops().push_str(&op);
    }
    /// Right-aligned at `xr`.
    fn rtext(&mut self, xr: f32, y: f32, size: f32, font: Font, color: (f32, f32, f32), s: &str) {
        let x = xr - text_width(s, size, font);
        self.text(x, y, size, font, color, s);
    }
    fn hline(&mut self, x1: f32, x2: f32, y: f32, color: (f32, f32, f32), width: f32) {
        let (r, g, b) = color;
        let op = format!("{r:.3} {g:.3} {b:.3} RG {width} w {x1:.2} {y:.2} m {x2:.2} {y:.2} l S\n");
        self.ops().push_str(&op);
    }
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: (f32, f32, f32)) {
        let (r, g, b) = color;
        let op = format!("{r:.3} {g:.3} {b:.3} rg {x:.2} {y:.2} {w:.2} {h:.2} re f\n");
        self.ops().push_str(&op);
    }
}

/// Columns of the line-item table (right edges for the number columns).
const COL_HOURS: f32 = 390.0;
const COL_RATE: f32 = 470.0;
const DESC_MAX: f32 = 300.0;

fn table_header(d: &mut Doc) {
    let y = d.y;
    d.text(LEFT, y, 8.5, Font::Regular, MUTED, "Description");
    d.rtext(COL_HOURS, y, 8.5, Font::Regular, MUTED, "Hours");
    d.rtext(COL_RATE, y, 8.5, Font::Regular, MUTED, "Rate");
    d.rtext(RIGHT, y, 8.5, Font::Regular, MUTED, "Amount");
    d.hline(LEFT, RIGHT, y - 7.0, LINE, 0.8);
    d.y -= 24.0;
}

/// The invoice as a PDF file. `from` is who it's from (the invoice's saved sender, or the
/// workspace's details for a draft).
pub fn invoice_pdf(inv: &Invoice, from: &SenderSnapshot) -> Vec<u8> {
    let mut d = Doc::new();
    let t = totals(inv);
    let m = |n: f64| money(n, &inv.currency);

    // Header: sender on the left, "Invoice" and number on the right.
    let business = if from.business_name.trim().is_empty() {
        "Your business"
    } else {
        from.business_name.trim()
    };
    d.text(LEFT, d.y, 17.0, Font::Bold, INK, business);
    d.rtext(RIGHT, d.y - 6.0, 28.0, Font::Bold, INK, "Invoice");
    let badge = if inv.status == "paid" {
        format!("{} · Paid", inv.number)
    } else {
        inv.number.clone()
    };
    d.rtext(RIGHT, d.y - 24.0, 10.0, Font::Regular, MUTED, &badge);
    let mut y = d.y - 18.0;
    for l in wrap(&from.address, 9.5, Font::Regular, 280.0)
        .iter()
        .chain([from.email.clone()].iter())
    {
        d.text(LEFT, y, 9.5, Font::Regular, MUTED, l);
        y -= 13.0;
    }
    d.y = y.min(d.y - 40.0) - 10.0;
    d.hline(LEFT, RIGHT, d.y, LINE, 0.8);
    d.y -= 28.0;

    // Billed to + dates.
    let top = d.y;
    d.text(LEFT, d.y, 8.5, Font::Regular, MUTED, "Billed to");
    d.y -= 15.0;
    d.text(LEFT, d.y, 11.0, Font::Bold, INK, &inv.client.name);
    d.y -= 14.0;
    let mut client_lines = Vec::new();
    if !inv.client.contact.is_empty() {
        client_lines.push(format!("Attn: {}", inv.client.contact));
    }
    client_lines.extend(wrap(&inv.client.address, 9.5, Font::Regular, 280.0));
    if !inv.client.email.is_empty() {
        client_lines.push(inv.client.email.clone());
    }
    for l in &client_lines {
        d.text(LEFT, d.y, 9.5, Font::Regular, MUTED, l);
        d.y -= 13.0;
    }
    let mut ry = top;
    for (label, value) in [
        ("Issued", long_date(inv.issue_date)),
        ("Due", long_date(inv.due_date)),
        ("Period", month_label(&inv.period)),
    ] {
        d.rtext(RIGHT, ry, 9.5, Font::Bold, INK, &value);
        d.rtext(
            RIGHT - text_width(&value, 9.5, Font::Bold) - 8.0,
            ry,
            9.5,
            Font::Regular,
            MUTED,
            label,
        );
        ry -= 15.0;
    }
    d.y = d.y.min(ry) - 22.0;

    // Line items.
    table_header(&mut d);
    for l in inv.lines.iter() {
        let desc = wrap(&l.desc, 10.0, Font::Bold, DESC_MAX);
        let detail = wrap(&l.detail, 8.5, Font::Regular, DESC_MAX);
        let height = desc.len() as f32 * 13.0 + detail.len() as f32 * 11.5 + 14.0;
        if d.room(height.min(TOP - BOTTOM - 40.0)) {
            table_header(&mut d);
        }
        let row = d.y;
        let hrs = if l.minutes > 0 {
            hours(l.minutes)
        } else {
            "-".into()
        };
        let rate = if l.kind == "hours" || l.kind == "overage" {
            format!("{}/h", m(l.rate))
        } else {
            "-".into()
        };
        d.rtext(COL_HOURS, row, 9.5, Font::Regular, INK, &hrs);
        d.rtext(COL_RATE, row, 9.5, Font::Regular, INK, &rate);
        d.rtext(RIGHT, row, 9.5, Font::Regular, INK, &m(l.amount));
        for s in &desc {
            d.text(LEFT, d.y, 10.0, Font::Bold, INK, s);
            d.y -= 13.0;
        }
        for s in &detail {
            d.text(LEFT, d.y + 1.0, 8.5, Font::Regular, MUTED, s);
            d.y -= 11.5;
        }
        d.hline(LEFT, RIGHT, d.y + 4.0, LINE, 0.6);
        d.y -= 14.0;
    }

    // Totals, right-aligned.
    let mut rows: Vec<(String, String, bool)> = vec![("Subtotal".into(), m(t.subtotal), false)];
    if t.discount > 0.0 {
        rows.push(("Discount".into(), format!("-{}", m(t.discount)), false));
    }
    if !inv.tax_rate.is_zero() {
        rows.push((
            format!("{} ({}%)", inv.tax_label, inv.tax_rate.normalize()),
            m(t.tax),
            false,
        ));
    }
    let settled = !inv.credits.is_empty() || !inv.payments.is_empty();
    if settled {
        rows.push(("Total".into(), m(t.total), true));
        for c in inv.credits.iter() {
            rows.push((
                format!("Credit note {}", c.number),
                format!("-{}", m(c.amount)),
                false,
            ));
        }
        for p in inv.payments.iter() {
            rows.push((
                format!("Paid {}", long_date(p.date)),
                format!("-{}", m(p.amount)),
                false,
            ));
        }
    }
    d.room(rows.len() as f32 * 16.0 + 40.0);
    d.y -= 4.0;
    let label_x = RIGHT - 210.0;
    for (label, value, bold) in &rows {
        let f = if *bold { Font::Bold } else { Font::Regular };
        d.text(label_x, d.y, 9.5, f, if *bold { INK } else { MUTED }, label);
        d.rtext(RIGHT, d.y, 9.5, f, INK, value);
        d.y -= 16.0;
    }
    d.hline(label_x, RIGHT, d.y + 8.0, INK, 0.9);
    d.y -= 10.0;
    let due_label = if inv.status == "paid" {
        "Paid"
    } else if settled {
        "Balance due"
    } else {
        "Amount due"
    };
    d.text(label_x, d.y, 12.5, Font::Bold, INK, due_label);
    d.rtext(
        RIGHT,
        d.y,
        12.5,
        Font::Bold,
        INK,
        &m(if settled { t.balance } else { t.total }),
    );
    d.y -= 30.0;

    // How to pay.
    if !from.payment_info.trim().is_empty() {
        let lines = wrap(&from.payment_info, 9.5, Font::Regular, RIGHT - LEFT - 28.0);
        let h = 30.0 + lines.len() as f32 * 13.0;
        d.room(h + 10.0);
        d.rect(LEFT, d.y - h + 12.0, RIGHT - LEFT, h, PANEL);
        d.text(
            LEFT + 14.0,
            d.y - 4.0,
            8.5,
            Font::Regular,
            MUTED,
            "How to pay",
        );
        d.y -= 20.0;
        for l in &lines {
            d.text(LEFT + 14.0, d.y, 9.5, Font::Regular, INK, l);
            d.y -= 13.0;
        }
        d.y -= 22.0;
    }

    // Notes.
    if !inv.notes.trim().is_empty() {
        for l in wrap(&inv.notes, 9.5, Font::Regular, RIGHT - LEFT) {
            d.room(14.0);
            d.text(LEFT, d.y, 9.5, Font::Regular, MUTED, &l);
            d.y -= 13.0;
        }
    }

    // Footers, now that the page count is known.
    let n = d.pages.len();
    for (i, page) in d.pages.iter_mut().enumerate() {
        let mut f = Doc {
            pages: vec![String::new()],
            y: 0.0,
        };
        f.hline(LEFT, RIGHT, 44.0, LINE, 0.6);
        f.text(
            LEFT,
            30.0,
            8.0,
            Font::Regular,
            MUTED,
            &format!("Invoice {} · {}", inv.number, business),
        );
        f.rtext(
            RIGHT,
            30.0,
            8.0,
            Font::Regular,
            MUTED,
            &format!("Page {} of {n}", i + 1),
        );
        page.push_str(&f.pages[0]);
    }

    assemble(&d.pages, &format!("Invoice {}", inv.number))
}

/* ---------------- File structure ---------------- */

fn pdf_string(s: &str) -> String {
    let hex: String = s.chars().map(|c| format!("{:02X}", win_ansi(c))).collect();
    format!("<{hex}>")
}

/// Objects: 1 catalog, 2 page tree, 3–4 fonts, 5 info, then a page + content pair per page.
fn assemble(pages: &[String], title: &str) -> Vec<u8> {
    let mut objects: Vec<String> = Vec::new();
    let page_ids: Vec<usize> = (0..pages.len()).map(|i| 6 + i * 2).collect();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        page_ids
            .iter()
            .map(|i| format!("{i} 0 R"))
            .collect::<Vec<_>>()
            .join(" "),
        pages.len()
    ));
    for (name, font) in [("Helvetica", Font::Regular), ("Helvetica-Bold", Font::Bold)] {
        let widths: Vec<String> = (32u8..=255)
            .map(|b| glyph_width(b, font).to_string())
            .collect();
        objects.push(format!(
            "<< /Type /Font /Subtype /Type1 /BaseFont /{name} /Encoding /WinAnsiEncoding /FirstChar 32 /LastChar 255 /Widths [{}] >>",
            widths.join(" ")
        ));
    }
    objects.push(format!(
        "<< /Title {} /Producer (TaskX) >>",
        pdf_string(title)
    ));
    for (i, content) in pages.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {W} {H}] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
            page_ids[i] + 1
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ));
    }

    let mut out: Vec<u8> = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, o) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info 5 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::models::{InvoiceLine, PartySnapshot};
    use sqlx::types::Json;

    pub fn sample(lines: usize) -> Invoice {
        Invoice {
            id: "i1".into(),
            seq: 1,
            number: "INV-0007".into(),
            client_id: "c1".into(),
            client: Json(PartySnapshot {
                name: "Acme Café".into(),
                contact: "Jo".into(),
                email: "jo@acme.test".into(),
                address: "1 Main St\nSpringfield".into(),
            }),
            sender: None,
            period: "2026-10".into(),
            issue_date: NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            due_date: NaiveDate::from_ymd_opt(2026, 11, 15).unwrap(),
            currency: "EUR".into(),
            lines: Json(
                (0..lines)
                    .map(|i| InvoiceLine {
                        kind: "hours".into(),
                        desc: format!("Design work, part {i} with a longer description that needs to wrap onto a second line"),
                        detail: "12 tasks".into(),
                        minutes: 90,
                        rate: 80.0,
                        amount: 120.0,
                        task_ids: vec![],
                        expense_ids: None,
                    })
                    .collect(),
            ),
            discount: Default::default(),
            tax_rate: "20".parse().unwrap(),
            tax_label: "VAT".into(),
            notes: "Thanks!".into(),
            status: "sent".into(),
            sent_at: None,
            paid_at: None,
            paid_note: None,
            credits: Json(vec![]),
            payments: Json(vec![]),
            created_at: 0,
            sample: false,
        }
    }

    /// Checks the file layout on the raw bytes (the header has binary bytes, so offsets
    /// must not go through UTF-8). Returns the page count.
    fn check_structure(pdf: &[u8]) -> usize {
        let ascii = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        assert!(pdf.starts_with(b"%PDF-1.4\n"));
        let tail = ascii(&pdf[pdf.len() - 64.min(pdf.len())..]);
        assert!(tail.trim_end().ends_with("%%EOF"));
        // startxref points at the xref table, and each entry at its object.
        let start: usize = tail
            .rsplit("startxref\n")
            .next()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(pdf[start..].starts_with(b"xref\n"));
        let xref = ascii(&pdf[start..]);
        let entries: Vec<&str> = xref
            .lines()
            .skip(3)
            .take_while(|l| l.ends_with(" n "))
            .collect();
        assert!(!entries.is_empty());
        for (i, entry) in entries.iter().enumerate() {
            let off: usize = entry[..10].parse().unwrap();
            assert!(
                pdf[off..].starts_with(format!("{} 0 obj", i + 1).as_bytes()),
                "object {}",
                i + 1
            );
        }
        // Every stream's /Length matches its bytes.
        let body = ascii(pdf);
        for part in body.split("<< /Length ").skip(1) {
            let len: usize = part.split_whitespace().next().unwrap().parse().unwrap();
            let data = part.split_once("stream\n").unwrap().1;
            assert!(data[len..].starts_with("endstream"), "stream length");
        }
        body.matches("/Type /Page ").count()
    }

    #[test]
    fn invoices_render_as_valid_pdfs() {
        let pdf = invoice_pdf(
            &sample(3),
            &SenderSnapshot {
                business_name: "Studio".into(),
                ..Default::default()
            },
        );
        assert_eq!(check_structure(&pdf), 1);
        let hex = |s: &str| s.bytes().map(|b| format!("{b:02X}")).collect::<String>();
        let s = String::from_utf8_lossy(&pdf);
        assert!(s.contains(&hex("INV-0007")));
        assert!(s.contains(&hex("Amount due")));
    }

    #[test]
    fn long_invoices_flow_onto_more_pages() {
        let mut inv = sample(60);
        inv.payments = Json(vec![crate::models::Payment {
            id: "p1".into(),
            date: NaiveDate::from_ymd_opt(2026, 11, 20).unwrap(),
            amount: 500.0,
            method: "Card".into(),
            note: String::new(),
        }]);
        let from = SenderSnapshot {
            business_name: "Atelier Zoë".into(),
            address: "12 Rue de la Paix\n75002 Paris".into(),
            email: "zoe@atelier.test".into(),
            payment_info: "IBAN FR76 3000 6000 0112 3456 7890 189 — BIC AGRIFRPP".into(),
            ..Default::default()
        };
        let pdf = invoice_pdf(&inv, &from);
        if let Ok(dir) = std::env::var("PDF_OUT") {
            std::fs::write(format!("{dir}/long.pdf"), &pdf).unwrap();
        }
        let pages = check_structure(&pdf);
        assert!(pages >= 3, "{pages} pages");
        assert!(String::from_utf8_lossy(&pdf).contains(
            &"Page 1 of "
                .bytes()
                .map(|b| format!("{b:02X}"))
                .collect::<String>()
        ));
    }

    #[test]
    fn money_and_text_helpers() {
        assert_eq!(money(1234.5, "USD"), "$1,234.50");
        assert_eq!(money(12000.0, "JPY"), "¥12,000");
        assert_eq!(money(-80.0, "EUR"), "-€80.00");
        assert_eq!(money(1000.0, "BDT"), "BDT 1,000.00");
        assert_eq!(win_ansi('€'), 0x80);
        assert_eq!(win_ansi('é'), 0xE9);
        assert_eq!(win_ansi('৳'), b'?');
        let lines = wrap("one two three four five six", 10.0, Font::Regular, 60.0);
        assert!(
            lines.len() > 1
                && lines
                    .iter()
                    .all(|l| text_width(l, 10.0, Font::Regular) <= 60.0)
        );
        assert_eq!(month_label("2026-10"), "October 2026");
        assert_eq!(hours(90), "1:30");
    }

    #[test]
    fn totals_match_the_app() {
        let t = totals(&sample(3));
        assert_eq!(
            (t.subtotal, t.tax, t.total, t.balance),
            (360.0, 72.0, 432.0, 432.0)
        );
    }
}
