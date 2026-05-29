use std::path::Path;

use crate::core::{PackageId, PackageKey};

use super::collect::CollectedFiles;

#[must_use]
pub fn infer_package_display_name(nodes: &mega::Nodes, collected: &CollectedFiles<'_>) -> String {
    let roots = nodes.roots().collect::<Vec<_>>();
    roots
        .iter()
        .find(|root| root.kind().is_folder())
        .map(|root| root.name().to_string())
        .or_else(|| {
            (roots.len() == 1 && roots[0].kind().is_file())
                .then(|| stemmed_package_name(roots[0].name()))
        })
        .or_else(|| common_collected_root(collected))
        .unwrap_or_else(|| "root".to_string())
}

#[must_use]
pub fn infer_package_id(nodes: &mega::Nodes, collected: &CollectedFiles<'_>) -> PackageId {
    let display_name = infer_package_display_name(nodes, collected);
    PackageId::for_package_key(&PackageKey::new(display_name))
}

pub(super) fn single_file_package_path(file_name: &str) -> String {
    let package_name = stemmed_package_name(file_name);
    let mut path = String::with_capacity(package_name.len() + file_name.len() + 1);
    path.push_str(&package_name);
    path.push('/');
    path.push_str(file_name);
    path
}

fn common_collected_root(collected: &CollectedFiles<'_>) -> Option<String> {
    let mut roots = collected
        .to_download
        .iter()
        .chain(collected.completed.iter())
        .filter_map(|item| item.path.split('/').next())
        .filter(|root| !root.is_empty())
        .map(str::to_string);
    let first = roots.next()?;
    roots.all(|root| root == first).then_some(first)
}

fn stemmed_package_name(file_name: &str) -> String {
    Path::new(file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or(file_name)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_file_package_path_wraps_file_in_stemmed_folder() {
        assert_eq!(single_file_package_path("file.bin"), "file/file.bin");
        assert_eq!(
            single_file_package_path("archive.tar.gz"),
            "archive.tar/archive.tar.gz"
        );
    }

    #[test]
    fn stemmed_package_name_uses_file_name_without_extension() {
        assert_eq!(stemmed_package_name("file.bin"), "file");
        assert_eq!(stemmed_package_name("archive.tar.gz"), "archive.tar");
        assert_eq!(stemmed_package_name(".env"), ".env");
    }
}
