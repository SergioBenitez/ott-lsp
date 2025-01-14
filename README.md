# ott-lsp

A simple, mostly WIP (but working) LSP server for [ott]. To use with (neo)vim,
`cargo install --path .` then configure:

```lua
local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')

configs.ott_lsp = {
    default_config = {
        cmd = { 'ott-lsp' },
        filetypes = { 'ott' }
        root_dir = function()
            return vim.fn.getcwd()
        end,
        single_file_support = true,
    },
}

lspconfig.ott_lsp.setup()
```

[ott]: https://github.com/ott-lang/ott
