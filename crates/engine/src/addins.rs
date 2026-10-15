//! Excel add-ins (`.xlam`): workbooks whose workbook-level names every open workbook can use.
//!
//! Many add-ins ship reusable `LAMBDA` functions and constants as defined names. GridCraft loads
//! those, so `=AddTax(A1)` works in any workbook while the add-in is enabled. It never runs an
//! add-in's VBA (out of scope, like `.xlsm` macros): an add-in that carries a VBA project loads its
//! names and says the rest is unavailable. Names that refer to cells in the add-in's own sheets are
//! skipped, because evaluated from another workbook they would read the wrong cells.

use std::sync::Arc;

use gridcraft_formula::Expr;
use gridcraft_model::{DefinedName, Workbook};
use serde_json::{Value as Json, json};

use crate::{DocState, Result, Session};

/// Most names one add-in may contribute (input-sized: a hostile file can define millions).
pub const MAX_NAMES: usize = 10_000;
/// Most add-ins installed at once.
pub const MAX_ADDINS: usize = 255;

/// An installed add-in.
#[derive(Clone, Debug, PartialEq)]
pub struct Addin {
    /// Shown name: the file name without its extension (`Finance Tools`).
    pub name: String,
    /// The file it was read from; `None` when installed from bytes (the browser, MCP `base64`).
    pub path: Option<String>,
    pub enabled: bool,
    /// The names it adds: workbook scope, no cell references.
    pub functions: Vec<DefinedName>,
    /// What didn't load and why, for the Add-ins dialog.
    pub warnings: Vec<String>,
}

/// An installed add-in as remembered between runs (`prefs.json`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddinPref {
    pub path: String,
    pub enabled: bool,
}

impl Addin {
    pub fn info(&self) -> Json {
        json!({
            "name": self.name,
            "path": self.path,
            "enabled": self.enabled,
            "functions": self.functions.iter().map(|n| json!({"name": n.name, "formula": n.formula, "comment": n.comment})).collect::<Vec<_>>(),
            "warnings": self.warnings,
        })
    }
    /// An add-in whose file can't be read: listed with the reason, so it can be removed.
    pub fn unreadable(path: &str, enabled: bool, why: String) -> Addin {
        Addin { name: display_name(path), path: Some(path.to_string()), enabled, functions: vec![], warnings: vec![why] }
    }
}

/// The shown name of an add-in file: its file name without the extension.
pub fn display_name(file_name: &str) -> String {
    let stem = std::path::Path::new(file_name).file_stem().and_then(|s| s.to_str()).unwrap_or("").trim();
    if stem.is_empty() { "Add-in".to_string() } else { stem.to_string() }
}

/// Reads an add-in from a file's bytes (`.xlam`, or any workbook GridCraft opens).
pub fn read(file_name: &str, bytes: &[u8]) -> Result<Addin> {
    let (wb, _) = crate::io::open_bytes(file_name, bytes)?;
    Ok(from_workbook(display_name(file_name), &wb))
}

/// The add-in a workbook makes: its workbook-scope names that hold no cell references.
pub fn from_workbook(name: String, wb: &Workbook) -> Addin {
    let mut functions: Vec<DefinedName> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();
    let mut too_many = false;
    for n in &wb.names {
        // Sheet-level names belong to the add-in's sheets; `_xlnm.`/`_xlfn.` names are Excel's.
        if n.scope.is_some() || n.name.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("_xl")) {
            continue;
        }
        let usable = gridcraft_formula::parse(&n.formula).is_ok_and(|e| !refers_to_cells(&e));
        if !usable {
            skipped.push(&n.name);
            continue;
        }
        if functions.iter().any(|f| f.name.eq_ignore_ascii_case(&n.name)) {
            continue;
        }
        if functions.len() >= MAX_NAMES {
            too_many = true;
            break;
        }
        functions.push(DefinedName { scope: None, ..n.clone() });
    }
    let mut warnings = Vec::new();
    if wb.vba_project.is_some() {
        warnings.push(
            "This add-in contains VBA macros. GridCraft doesn't run macros, so commands and functions written in VBA aren't available.".to_string(),
        );
    }
    if !skipped.is_empty() {
        let shown: Vec<&str> = skipped.iter().take(5).copied().collect();
        let more = if skipped.len() > shown.len() { format!(" and {} more", skipped.len() - shown.len()) } else { String::new() };
        warnings.push(format!(
            "{} name{} refer{} to cells in the add-in's own sheets and {} skipped: {}{more}.",
            skipped.len(),
            if skipped.len() == 1 { "" } else { "s" },
            if skipped.len() == 1 { "s" } else { "" },
            if skipped.len() == 1 { "was" } else { "were" },
            shown.join(", "),
        ));
    }
    if too_many {
        warnings.push(format!("Only the first {MAX_NAMES} names were loaded."));
    }
    if functions.is_empty() {
        warnings.push("This add-in has no LAMBDA functions or constants GridCraft can use.".to_string());
    }
    Addin { name, path: None, enabled: true, functions, warnings }
}

/// Whether a name's formula reads cells: references, table references or `Sheet!Name`.
fn refers_to_cells(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if matches!(x, Expr::Ref(_) | Expr::Struct(_)) || matches!(x, Expr::Name(n) if n.contains('!')) {
            found = true;
        }
    });
    found
}

/// Gives a document the session's add-in names, recalculating it if they changed. A workbook that
/// had no unsaved changes still has none: add-in results aren't the user's edits.
pub(crate) fn apply(d: &mut DocState, names: &Arc<Vec<DefinedName>>) {
    if Arc::ptr_eq(&d.wb.addin_names, names) || (d.wb.addin_names.is_empty() && names.is_empty()) {
        return;
    }
    let clean = !d.is_dirty();
    let wb = Arc::make_mut(&mut d.wb);
    wb.addin_names = names.clone();
    d.calc.recalc_all(wb);
    if clean {
        d.saved = d.wb.clone();
    }
    d.revision += 1;
}

impl Session {
    /// The enabled add-ins' names, first add-in first (an earlier add-in's name wins).
    pub fn addin_names(&self) -> &Arc<Vec<DefinedName>> {
        &self.addin_names
    }

    /// Rebuilds the add-in names after an add-in changed, gives them to every open workbook and
    /// remembers the installed add-ins in the preferences.
    pub fn sync_addins(&mut self) {
        let mut names: Vec<DefinedName> = Vec::new();
        for a in self.addins.iter().filter(|a| a.enabled) {
            for f in &a.functions {
                if !names.iter().any(|n| n.name.eq_ignore_ascii_case(&f.name)) {
                    names.push(DefinedName { comment: if f.comment.is_empty() { a.name.clone() } else { f.comment.clone() }, ..f.clone() });
                }
            }
        }
        if *self.addin_names != names {
            self.addin_names = Arc::new(names);
        }
        let names = self.addin_names.clone();
        for d in self.docs.iter_mut() {
            apply(d, &names);
        }
        self.prefs.addins = self.addins.iter().filter_map(|a| Some(AddinPref { path: a.path.clone()?, enabled: a.enabled })).collect();
    }

    /// Adds an add-in, replacing an installed one with the same file (or, without a file, the
    /// same name). Returns its index.
    pub fn install_addin(&mut self, addin: Addin) -> Result<usize> {
        let same = |a: &Addin| match (&a.path, &addin.path) {
            (Some(x), Some(y)) => x == y,
            (None, None) => a.name.eq_ignore_ascii_case(&addin.name),
            _ => false,
        };
        let i = match self.addins.iter().position(same) {
            Some(i) => {
                if let Some(slot) = self.addins.get_mut(i) {
                    *slot = addin;
                }
                i
            }
            None => {
                if self.addins.len() >= MAX_ADDINS {
                    return Err(crate::EngineError::Other(format!("GridCraft can't install more than {MAX_ADDINS} add-ins. Remove one first.")));
                }
                self.addins.push(addin);
                self.addins.len() - 1
            }
        };
        self.sync_addins();
        Ok(i)
    }

    /// Loads the add-ins the preferences remember (at startup, after reading `prefs.json`). One that
    /// can't be read stays listed with the reason. Returns those reasons, for the log.
    pub fn load_installed_addins(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        let remembered: Vec<AddinPref> = self.prefs.addins.iter().take(MAX_ADDINS).cloned().collect();
        self.addins = remembered
            .into_iter()
            .map(|p| {
                let loaded = crate::io::read_file(&p.path).and_then(|bytes| read(&p.path, &bytes));
                match loaded {
                    Ok(a) => Addin { path: Some(p.path), enabled: p.enabled, ..a },
                    Err(e) => {
                        let why = format!("This add-in couldn't be loaded: {e}");
                        problems.push(format!("{}: {why}", p.path));
                        Addin::unreadable(&p.path, p.enabled, why)
                    }
                }
            })
            .collect();
        self.sync_addins();
        problems
    }
}

#[cfg(test)]
mod tests {
    use gridcraft_core::{CellError, CellRef, Value};
    use gridcraft_model::{DefinedName, Workbook};
    use serde_json::json;

    use crate::Session;

    fn name(name: &str, scope: Option<usize>, formula: &str) -> DefinedName {
        DefinedName { name: name.into(), scope, formula: formula.into(), comment: String::new(), hidden: false }
    }

    /// An add-in file: two functions and a constant, plus names GridCraft must not load.
    fn tools(file: &str, vba: bool) -> String {
        let mut wb = Workbook::new();
        wb.names = vec![
            name("Twice", None, "LAMBDA(x,x*2)"),
            name("AddTax", None, "LAMBDA(x,x*(1+TaxRate))"),
            name("TaxRate", None, "0.25"),
            name("Lookup", None, "Sheet1!$A$1:$B$9"),
            name("Local", Some(0), "LAMBDA(x,x)"),
            name("_xlnm.Print_Area", None, "Sheet1!$A$1:$C$3"),
        ];
        if vba {
            wb.vba_project = Some(b"not really VBA".to_vec());
        }
        crate::io::base64_encode(&crate::io::save_bytes(&wb, file).unwrap())
    }

    fn v(s: &Session, a: &str) -> Value {
        s.doc().unwrap().wb.active().unwrap().value(CellRef::parse(a).unwrap())
    }

    fn set(s: &mut Session, a: &str, input: &str) {
        s.execute("cell.set", json!({"cell": a, "input": input})).unwrap();
    }

    #[test]
    fn installed_functions_work_in_every_workbook() {
        let mut s = Session::new();
        s.new_workbook();
        set(&mut s, "A1", "=Twice(21)");
        assert_eq!(v(&s, "A1"), Value::Error(CellError::Name));
        let r = s.execute("addins.install", json!({"name": "Finance Tools.xlam", "base64": tools("t.xlsx", false)})).unwrap();
        assert_eq!(r["name"], "Finance Tools");
        let loaded: Vec<&str> = r["functions"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(loaded, ["Twice", "AddTax", "TaxRate"]);
        let warnings = r["warnings"].to_string();
        assert!(warnings.contains("1 name refers to cells") && warnings.contains("Lookup"), "{warnings}");
        // Existing formulas recalculate; new ones and other workbooks see the functions too.
        assert_eq!(v(&s, "A1"), Value::Number(42.0));
        set(&mut s, "A2", "=AddTax(100)");
        assert_eq!(v(&s, "A2"), Value::Number(125.0));
        s.new_workbook();
        set(&mut s, "A1", "=TaxRate*4");
        assert_eq!(v(&s, "A1"), Value::Number(1.0));
        // Listed by the Add-ins command.
        assert_eq!(s.execute("home.addIns", json!({})).unwrap()["addins"][0]["enabled"], true);
    }

    #[test]
    fn disabling_and_removing_bring_back_name_errors() {
        let mut s = Session::new();
        s.new_workbook();
        s.execute("addins.install", json!({"name": "Tools.xlam", "base64": tools("t.xlsx", false)})).unwrap();
        set(&mut s, "A1", "=Twice(5)");
        assert_eq!(v(&s, "A1"), Value::Number(10.0));
        s.execute("addins.setEnabled", json!({"name": "tools", "enabled": false})).unwrap();
        assert_eq!(v(&s, "A1"), Value::Error(CellError::Name));
        s.execute("addins.setEnabled", json!({"name": "Tools", "enabled": true})).unwrap();
        assert_eq!(v(&s, "A1"), Value::Number(10.0));
        s.execute("addins.remove", json!({"name": "Tools"})).unwrap();
        assert_eq!(v(&s, "A1"), Value::Error(CellError::Name));
        assert!(s.addins.is_empty());
        assert!(s.execute("addins.remove", json!({"name": "Tools"})).is_err());
        // Programmatic calls never open dialogs.
        assert!(s.execute("addins.install", json!({})).is_err());
    }

    #[test]
    fn installing_keeps_a_saved_workbook_saved_and_undo_keeps_the_addins() {
        let mut s = Session::new();
        s.new_workbook();
        set(&mut s, "A1", "=Twice(3)");
        let d = s.active_mut().unwrap();
        d.saved = d.wb.clone();
        s.execute("addins.install", json!({"name": "Tools.xlam", "base64": tools("t.xlsx", false)})).unwrap();
        assert!(!s.doc().unwrap().is_dirty());
        // Stepping back past the install, then forward again, still uses the add-in.
        s.execute("edit.undo", json!({})).unwrap();
        assert!(v(&s, "A1").is_empty());
        s.execute("edit.redo", json!({})).unwrap();
        assert_eq!(v(&s, "A1"), Value::Number(6.0));
        // Add-in names are never saved into the workbook.
        let saved = crate::io::save_bytes(&s.doc().unwrap().wb, "out.xlsx").unwrap();
        let (back, _) = crate::io::open_bytes("out.xlsx", &saved).unwrap();
        assert!(back.names.is_empty() && back.addin_names.is_empty());
    }

    #[test]
    fn vba_is_reported_not_run() {
        let mut s = Session::new();
        s.new_workbook();
        let r = s.execute("addins.install", json!({"name": "Macros.xlam", "base64": tools("t.xlsm", true)})).unwrap();
        assert!(r["warnings"].to_string().contains("doesn't run macros"));
        assert_eq!(r["functions"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn installed_addins_are_remembered_and_reloaded() {
        let dir = std::env::temp_dir().join(format!("gridcraft-addins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Tools.xlam");
        std::fs::write(&file, crate::io::base64_decode(&tools("t.xlsx", false)).unwrap()).unwrap();
        let path = file.to_str().unwrap().to_string();
        let mut s = Session::new();
        s.execute("addins.install", json!({"path": path, "enabled": false})).unwrap();
        assert_eq!(s.prefs.addins, [super::AddinPref { path: path.clone(), enabled: false }]);
        // Installing the same file again replaces it.
        s.execute("addins.install", json!({"path": path})).unwrap();
        assert_eq!(s.addins.len(), 1);
        // The next run reads the remembered file; a missing one stays listed with the reason.
        let mut prefs = s.prefs.clone();
        prefs.addins.push(super::AddinPref { path: dir.join("Gone.xlam").to_str().unwrap().into(), enabled: true });
        let mut next = Session::new();
        next.prefs = prefs;
        let problems = next.load_installed_addins();
        assert_eq!(problems.len(), 1);
        assert_eq!(next.addins.len(), 2);
        assert_eq!(next.addins[1].name, "Gone");
        next.new_workbook();
        set(&mut next, "A1", "=Twice(4)");
        assert_eq!(v(&next, "A1"), Value::Number(8.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
