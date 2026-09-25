# UI translations

Slint bundles gettext catalogs into the application at build time. English is the source
language; each locale lives at `<locale>/LC_MESSAGES/dogi.po`.

Update the template after changing an `@tr(...)` string:

```sh
package_version="$(cargo pkgid | sed 's/.*#//')"
slint-tr-extractor \
  --no-default-translation-context \
  --package-name dogi \
  --package-version "$package_version" \
  --default-domain dogi \
  -o src/ui/i18n/dogi.pot \
  src/ui/views/app.slint \
  src/ui/views/controls.slint \
  src/ui/views/i18n.slint \
  src/ui/views/loading.slint
```

Merge the template into a locale catalog, translate every non-header entry, then run
`cargo check`. The Slint build validates and embeds the catalog.
