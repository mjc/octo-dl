use std::fmt;

use unicode_width::UnicodeWidthStr;

pub(super) enum PackageName<'a> {
    Borrowed(&'a str),
    Prefixed {
        prefix: &'static str,
        value: &'a str,
    },
}

impl<'a> PackageName<'a> {
    pub(super) fn new(display_name: &'a str, folder_label: Option<&'a str>) -> Self {
        if !display_name.starts_with("http://") && !display_name.starts_with("https://") {
            return Self::Borrowed(compact_label(display_name));
        }
        if let Some(label) = folder_label {
            return Self::Borrowed(label);
        }
        if let Some(name) = mega_url_name(display_name) {
            return name;
        }
        Self::Borrowed(compact_label(
            display_name.split('#').next().unwrap_or(display_name),
        ))
    }

    pub(super) const fn parts(&self) -> (&str, &str) {
        match self {
            Self::Borrowed(value) => ("", value),
            Self::Prefixed { prefix, value } => (prefix, value),
        }
    }

    pub(super) fn width(&self) -> usize {
        let (prefix, value) = self.parts();
        UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(value)
    }
}

impl fmt::Display for PackageName<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (prefix, value) = self.parts();
        formatter.write_str(prefix)?;
        formatter.write_str(value)
    }
}

pub(super) fn compact_label(value: &str) -> &str {
    value
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
}

fn mega_url_name(value: &str) -> Option<PackageName<'_>> {
    let marker = "mega.nz/";
    let start = value.find(marker)? + marker.len();
    let path = &value[start..];
    let mut parts = path.split(['/', '#']);
    match (parts.next(), parts.next()) {
        (Some("folder"), Some(id)) if !id.is_empty() => Some(PackageName::Prefixed {
            prefix: "Folder ",
            value: id,
        }),
        (Some("file"), Some(id)) if !id.is_empty() => Some(PackageName::Prefixed {
            prefix: "File ",
            value: id,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::PackageName;

    #[test]
    fn package_name_policy_handles_paths_urls_and_fragments() {
        let cases = [
            ("/downloads/Series Name", None, "Series Name"),
            ("C:\\downloads\\Series Name", None, "Series Name"),
            ("https://mega.nz/folder/abc#secret", None, "Folder abc"),
            ("https://mega.nz/file/def#secret", None, "File def"),
            ("https://example.test/path#fragment", None, "path"),
            ("https://mega.nz/folder/", None, "folder"),
            ("https://mega.nz/file/", None, "file"),
            (
                "https://mega.nz/folder/abc",
                Some("Configured"),
                "Configured",
            ),
        ];

        for (value, folder, expected) in cases {
            assert_eq!(PackageName::new(value, folder).to_string(), expected);
        }
    }
}
