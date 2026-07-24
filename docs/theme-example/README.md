# ReTheme 主题示例

`package/` 是可以直接复制、加载和压缩投稿的纯主题目录：

```text
package/
├── manifest.json
├── styles/
│   ├── tokens.css
│   └── overrides.css
└── assets/
    └── hero.svg
```

开发步骤：

1. 复制整个 `package/`，不要只复制单个 CSS。
2. 修改 `manifest.json` 的 `id`、名称、版本、作者、文案和预览色。
3. 先在 `tokens.css` 建立浅色与深色语义 token，再在 `overrides.css` 映射稳定插槽。
4. 使用 ReTheme 的“加载本地主题”选择复制后的目录。
5. 执行 `pnpm dlx @duxweb/retheme-theme-skill validate /path/to/theme`。
6. 只压缩 `package/` 内部内容，确保 ZIP 根目录直接出现 `manifest.json`。

字段解释见 [`manifest.annotated.jsonc`](manifest.annotated.jsonc)。该 JSONC 仅供阅读，不能放入实际主题包。CSS 中的注释可以保留，校验器会正常解析。

本示例故意保持视觉简单，展示协议边界而不是推荐某种风格。主题可以增加更多插槽和受控资源，但不得修改 ChatGPT 结构布局、设置字体、使用内部类名或从 CSS 加载图片。

## 真实源包示例

[`caishen-readable/`](caishen-readable/) 提供一套更接近真实主题投稿的源码示例：包含生成 artwork、浅色可读界面 token、首页 Hero、会话 Banner、代码/终端插槽和中英文文案。它不是 ReTheme 桌面端内置主题，也不是签名 `.ctheme`，只作为主题作者验证协议、加载本地主题和准备源码 ZIP 投稿的参考。
