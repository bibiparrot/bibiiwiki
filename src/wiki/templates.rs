use anyhow::{Context, Result};
use include_dir::{Dir, include_dir};
use minijinja::{Environment, UndefinedBehavior};
use serde::Serialize;

static TEMPLATE_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/templates");

pub struct TemplateLibrary {
    environment: Environment<'static>,
    names: Vec<String>,
}

impl TemplateLibrary {
    /// Compiles all bundled Python-compatible Jinja templates.
    ///
    /// # Errors
    ///
    /// Returns an error if a bundled template is not UTF-8 or contains syntax
    /// unsupported by `MiniJinja`.
    pub fn load() -> Result<Self> {
        let mut environment = Environment::new();
        environment.set_undefined_behavior(UndefinedBehavior::Strict);
        let mut names = Vec::new();
        add_directory(&TEMPLATE_DIR, &mut environment, &mut names)?;
        names.sort();
        Ok(Self { environment, names })
    }

    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Renders a bundled template and normalizes it to one trailing newline.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown template or missing/invalid context.
    pub fn render<S: Serialize>(&self, name: &str, context: &S) -> Result<String> {
        let rendered = self
            .environment
            .get_template(name)
            .with_context(|| format!("unknown template {name:?}"))?
            .render(context)
            .with_context(|| format!("failed to render template {name:?}"))?;
        Ok(format!("{}\n", rendered.trim_end()))
    }
}

fn add_directory(
    directory: &'static Dir<'static>,
    environment: &mut Environment<'static>,
    names: &mut Vec<String>,
) -> Result<()> {
    for file in directory.files() {
        if file.path().extension().and_then(|value| value.to_str()) != Some("j2") {
            continue;
        }
        let name = file.path().to_string_lossy().replace('\\', "/");
        let source = file
            .contents_utf8()
            .with_context(|| format!("template {name:?} is not valid UTF-8"))?;
        environment
            .add_template_owned(name.clone(), source.to_string())
            .with_context(|| format!("failed to compile template {name:?}"))?;
        names.push(name);
    }
    for child in directory.dirs() {
        add_directory(child, environment, names)?;
    }
    Ok(())
}

impl std::fmt::Debug for TemplateLibrary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TemplateLibrary")
            .field("templates", &self.names.len())
            .finish_non_exhaustive()
    }
}
