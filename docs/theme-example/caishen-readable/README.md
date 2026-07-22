# Caishen Readable ReTheme source example

This directory contains a public-safe ReTheme source package adapted from the
Caishen Readable Codex skin pack.

It is not bundled into the ReTheme desktop app and it is not a signed `.ctheme`
package. It exists as a complete source example for authors who want to test a
realistic visual direction with the ReTheme protocol, local loading, and the
shared validator.

## Package

```text
package/
├── manifest.json
├── styles/
│   ├── tokens.css
│   └── overrides.css
└── assets/
    └── hero.jpg
```

## Validate

```bash
pnpm dlx @duxweb/retheme-theme-skill validate docs/theme-example/caishen-readable/package
```

To validate the submission ZIP, compress only the contents of `package/` so the
ZIP root contains `manifest.json` directly.

## Source and privacy

- Theme source: <https://github.com/ChannelerH/codex-skin-packs>
- Preview page: <https://codex-theme-gallery.howardhua.chatgpt.site/themes/caishen-readable?utm_source=duxweb-retheme&utm_medium=github-pr&utm_campaign=caishen-readable>
- The artwork is generated for a public theme pack.
- No private Codex workspace screenshots, task names, chats, file paths, or user data are included.

