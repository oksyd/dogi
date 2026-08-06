# UI translations

Slint bundles gettext catalogs into the application at build time. English is the source
language; each locale lives at `<locale>/LC_MESSAGES/dogi-ui.po`.

Update the template after changing an `@tr(...)` string:

```sh
package_version="$(cargo pkgid -p dogi-ui | sed 's/.*#//')"
slint-tr-extractor \
  --no-default-translation-context \
  --package-name dogi-ui \
  --package-version "$package_version" \
  --default-domain dogi-ui \
  -o crates/dogi-ui/i18n/dogi-ui.pot \
  crates/dogi-ui/ui/app.slint \
  crates/dogi-ui/ui/controls.slint \
  crates/dogi-ui/ui/i18n.slint \
  crates/dogi-ui/ui/loading.slint
```

Merge the template into a locale catalog, translate every non-header entry, then run
`cargo check -p dogi-ui`. The Slint build validates and embeds the catalog.
