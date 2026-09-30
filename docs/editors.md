# Editors

`magi lsp` is a language server: an editor starts it and talks to it over stdin and stdout
(the Language Server Protocol). Any editor with an LSP client can use it.

## What the server does

- **Diagnostics**: the errors, warnings and notes of `magi check`, with their codes
  ([`diagnostics.md`](diagnostics.md)), updated on every change to a buffer, not only on save.
  SQL sources are not contacted; local files are read to learn their columns. Imported files
  are read from the editor's buffers when they are open, else from disk. A file that another
  `.magi` file in the same directory imports is checked as part of that program, so names it
  uses from the importing file are not reported as unknown. Diagnostics in imported files are
  shown in those files.
- **Formatting**: the whole document in the style of `magi fmt`. A document that does not parse
  is left as it is.
- **Go to definition** of a source, dataset, reconcile, mapping or connection name, also when it
  is declared in an imported file. `rec.matches` leads to the reconcile `rec`.
- **Hover** on such a name: its kind, where it is declared, and the columns with their types (for
  a reconcile, its output relations).
- **Completion**: statement keywords at the start of a line, the outputs of `name.` (such as
  `rec.matches`), and else the program's relations, mappings, connections and functions.
- **Semantic highlighting**: comments, strings, numbers, keywords and function calls, for
  editors that have no MAGI grammar.

The server writes its own errors to stderr, which most editors show in an LSP log.

## VS Code

VS Code needs an extension to start a language server. Without a MAGI extension, use a generic
LSP client such as [Generic LSP Client (v2)](https://marketplace.visualstudio.com/items?itemName=zsol.vscode-glspc)
and set in `settings.json`:

```json
{
    "files.associations": { "*.magi": "magi" },
    "glspc.server.command": "magi",
    "glspc.server.commandArguments": ["lsp"],
    "glspc.server.languageId": ["magi"]
}
```

`magi` must be on the `PATH` VS Code sees (else give the full path in `glspc.server.command`).

## Neovim

In `init.lua` (Neovim 0.10 or later):

```lua
vim.filetype.add({ extension = { magi = "magi" } })

vim.api.nvim_create_autocmd("FileType", {
    pattern = "magi",
    callback = function(args)
        vim.lsp.start({
            name = "magi",
            cmd = { "magi", "lsp" },
            root_dir = vim.fs.dirname(args.file),
        })
    end,
})
```

Format with `:lua vim.lsp.buf.format()`; `gd`-style mappings and `K` for hover work as for any
language server.

## Helix

In `~/.config/helix/languages.toml`:

```toml
[language-server.magi]
command = "magi"
args = ["lsp"]

[[language]]
name = "magi"
scope = "source.magi"
file-types = ["magi"]
comment-token = "#"
roots = []
language-servers = ["magi"]
```

Helix highlights with tree-sitter grammars, and there is no MAGI grammar; a Helix release that
does not use LSP semantic tokens shows MAGI files without highlighting. The other features work.
