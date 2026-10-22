//! The one look every email shares (design board 2026-10-22-polish): the app icon and name on
//! top, a white card with the Spark's two-tone seam along its top edge, one orange button with
//! white text, and a short footer. Tables and inline styles only (mail apps drop `<style>` and
//! scripts), solid colours that still read when a mail app's dark mode inverts them.
//!
//! Every function takes text that is already escaped where it says `_html`; plain `&str`
//! arguments are escaped here.

use super::report::esc;

pub const INK: &str = "#0e1014";
pub const MUTED: &str = "#4a505b";
pub const FAINT: &str = "#5f6672";
pub const LINE: &str = "#e3e6eb";
pub const PAGE: &str = "#f2f3f6";
pub const SOFT: &str = "#f7f8fa";
/// Molten as text (5:1 on white).
pub const MOLTEN_TEXT: &str = "#9a5400";
/// Molten on the seam and the best-stream edge.
pub const MOLTEN: &str = "#cf7400";
pub const ARC: &str = "#0888b8";
/// Arc as text: links.
pub const ARC_TEXT: &str = "#05678c";
/// The button: deep Molten with white text (5.6:1).
pub const BUTTON: &str = "#b35900";
pub const UP: &str = "#0a6b46";
pub const DOWN: &str = "#c42339";
pub const FONT: &str = "font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
pub const MONO: &str = "font-family:'JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,Consolas,monospace";

/// The site that hosts the icon and the help links.
const SITE: &str = "https://ferrocast.clearcraftlabs.com";

/// The sender's postal address under every email (anti-spam laws ask for one). The same
/// registered address as the website's legal pages (`apps/site/src/data/company.ts`).
pub const POSTAL_ADDRESS: &str = "ClearCraft Labs, Jandiala Guru, Amritsar, Punjab 143115, India";

/// A paragraph in the card's body text.
pub fn para(text_html: &str) -> String {
    format!("<p style=\"margin:0 0 18px;{FONT};font-size:16px;line-height:24px;color:{MUTED}\">{text_html}</p>")
}

/// The small coloured line above a title ("Weekly report").
pub fn eyebrow(text: &str) -> String {
    format!(
        "<p style=\"margin:0 0 8px;{FONT};font-size:12px;line-height:16px;font-weight:700;letter-spacing:.08em;text-transform:uppercase;color:{MOLTEN_TEXT}\">{}</p>",
        esc(text)
    )
}

pub fn title(text: &str) -> String {
    format!(
        "<h1 style=\"margin:0 0 10px;{FONT};font-size:26px;line-height:32px;font-weight:800;letter-spacing:-.02em;color:{INK}\">{}</h1>",
        esc(text)
    )
}

/// The one button. A table cell carries the colour so Outlook draws it too.
pub fn button(href: &str, label: &str) -> String {
    format!(
        "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr>\
         <td style=\"border-radius:10px;background:{BUTTON}\">\
         <a href=\"{h}\" style=\"display:inline-block;padding:14px 26px;border-radius:10px;background:{BUTTON};color:#ffffff;{FONT};font-size:16px;line-height:20px;font-weight:700;text-decoration:none\">{l}</a>\
         </td></tr></table>",
        h = esc(href),
        l = esc(label),
    )
}

/// A one-time code in a big dashed box, easy to read and copy.
pub fn code_box(caption: &str, code: &str) -> String {
    format!(
        "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"margin:22px 0 4px\"><tr>\
         <td align=\"center\" style=\"padding:16px;border:1px dashed #c9ced7;border-radius:12px;background:{SOFT}\">\
         <div style=\"{FONT};font-size:13px;line-height:18px;color:{FAINT};margin-bottom:6px\">{c}</div>\
         <div style=\"{MONO};font-size:30px;line-height:36px;font-weight:700;letter-spacing:.16em;color:{INK}\">{k}</div>\
         </td></tr></table>",
        c = esc(caption),
        k = esc(code),
    )
}

/// The quiet line under a hairline at the end of the card (security or "ignore this").
pub fn note(text: &str) -> String {
    format!(
        "<p style=\"margin:22px 0 0;padding-top:18px;border-top:1px solid {LINE};{FONT};font-size:13.5px;line-height:20px;color:{FAINT}\">{}</p>",
        esc(text)
    )
}

/// The whole email. `preheader` is the grey preview line mail apps show next to the subject.
pub fn page(subject: &str, preheader: &str, body_html: &str, footer_html: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light\"><meta name=\"supported-color-schemes\" content=\"light\">\
         <title>{subject_e}</title></head>\
         <body style=\"margin:0;padding:0;background:{PAGE}\">\
         <div style=\"display:none;max-height:0;overflow:hidden;color:{PAGE}\">{pre_e}</div>\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"background:{PAGE}\"><tr><td align=\"center\" style=\"padding:28px 12px 32px\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"max-width:540px\">\
         <tr><td style=\"padding:0 4px 16px\">\
         <table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr>\
         <td style=\"padding-right:8px\"><img src=\"{SITE}/icon-192.png\" width=\"28\" height=\"28\" alt=\"\" style=\"display:block;border:0;border-radius:7px\"></td>\
         <td style=\"{FONT};font-size:17px;line-height:20px;font-weight:800;letter-spacing:-.02em;color:{INK}\">Ferrocast</td>\
         </tr></table></td></tr>\
         <tr><td style=\"background:#ffffff;border:1px solid {LINE};border-radius:16px;overflow:hidden\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr>\
         <td width=\"50%\" style=\"height:4px;line-height:4px;font-size:0;background:{MOLTEN};border-top-left-radius:16px\">&nbsp;</td>\
         <td width=\"50%\" style=\"height:4px;line-height:4px;font-size:0;background:{ARC};border-top-right-radius:16px\">&nbsp;</td>\
         </tr></table>\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr><td style=\"padding:30px 28px 28px\">{body_html}</td></tr></table>\
         </td></tr>\
         <tr><td align=\"center\" style=\"padding:18px 8px 0;{FONT};font-size:12.5px;line-height:19px;color:{FAINT}\">{footer_html}<br>{POSTAL_ADDRESS}</td></tr>\
         </table></td></tr></table></body></html>",
        subject_e = esc(subject),
        pre_e = esc(preheader),
    )
}

/// A link in the footer.
pub fn footer_link(href: &str, label: &str) -> String {
    format!("<a href=\"{}\" style=\"color:{ARC_TEXT};text-decoration:underline\">{}</a>", esc(href), esc(label))
}

/// The usual footer line: who sends it, then the site and help.
pub fn footer(lead: &str) -> String {
    format!(
        "{}<br>{} · {}",
        esc(lead),
        footer_link(SITE, "ferrocast.clearcraftlabs.com"),
        footer_link(&format!("{SITE}/contact/"), "Help")
    )
}
