//! Add-ins: install, enable, disable and remove `.xlam` add-ins (see [`crate::addins`]).

use serde_json::{Value as Json, json};

use super::*;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "home.addIns",
            "Add-ins",
            ["Home", "Add-ins"],
            None,
            "{} → {addins: [{name, path, enabled, functions: [{name, formula, comment}], warnings}]} (the interface opens the Add-ins dialog)",
            always,
            list
        ),
        cmd!(
            noundo "addins.install",
            "Install Add-in",
            [],
            None,
            "{path} | {name, base64}, {enabled?: bool} → the add-in. Loads the LAMBDA functions and constants of an .xlam (or any workbook) for every open workbook; VBA is never run",
            always,
            install
        ),
        cmd!(noundo "addins.setEnabled", "Enable Add-in", [], None, "{name, enabled: bool}", always, set_enabled),
        cmd!(noundo "addins.remove", "Remove Add-in", [], None, "{name}", always, remove),
    ]
}

fn list(s: &mut Session, _: &Json) -> Result<Json> {
    Ok(json!({"addins": s.addins.iter().map(crate::addins::Addin::info).collect::<Vec<_>>()}))
}

fn install(s: &mut Session, p: &Json) -> Result<Json> {
    let (name, path, bytes) = if let Some(b) = str_param(p, "base64") {
        let name = str_param(p, "name").unwrap_or("Add-in.xlam").to_string();
        (name, None, crate::io::base64_decode(b).ok_or_else(|| bad("addins.install", "invalid base64"))?)
    } else if let Some(path) = str_param(p, "path") {
        (path.to_string(), Some(path.to_string()), crate::io::read_file(path)?)
    } else {
        return Err(bad("addins.install", "missing `path` or `base64`: programmatic calls never open dialogs"));
    };
    let mut addin = crate::addins::read(&name, &bytes)?;
    addin.path = path;
    addin.enabled = bool_param(p, "enabled").unwrap_or(true);
    let i = s.install_addin(addin)?;
    Ok(s.addins.get(i).map(crate::addins::Addin::info).unwrap_or(Json::Null))
}

/// The installed add-in a call names (`name`, case-insensitive).
fn named(s: &Session, p: &Json, cmd: &str) -> Result<usize> {
    let name = str_param(p, "name").ok_or_else(|| bad(cmd, "missing `name`"))?;
    s.addins.iter().position(|a| a.name.eq_ignore_ascii_case(name)).ok_or_else(|| bad(cmd, format!("no add-in named `{name}`")))
}

fn set_enabled(s: &mut Session, p: &Json) -> Result<Json> {
    let i = named(s, p, "addins.setEnabled")?;
    let enabled = bool_param(p, "enabled").ok_or_else(|| bad("addins.setEnabled", "missing `enabled`"))?;
    if let Some(a) = s.addins.get_mut(i) {
        a.enabled = enabled;
    }
    s.sync_addins();
    ok()
}

fn remove(s: &mut Session, p: &Json) -> Result<Json> {
    let i = named(s, p, "addins.remove")?;
    s.addins.remove(i);
    s.sync_addins();
    ok()
}
