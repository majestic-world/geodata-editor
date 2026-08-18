# Geodata Editor

Editor nativo para arquivos de geodata Lineage II (`.l2j`, `.l2g` e
`_conv.dat`).

## Distribuição Windows

```powershell
make editor
```

O executável público será criado em `dist\GeodataEditor.exe`. O perfil de
release usa LTO e remove símbolos de depuração antes da cópia.

## Licença

Este projeto é licenciado sob a **GNU General Public License, versão 3 ou
posterior** (`GPL-3.0-or-later`). O texto integral está em
[`LICENSE`](LICENSE).

Ao distribuir `GeodataEditor.exe`, distribua também o código-fonte
correspondente sob a mesma licença ou indique, junto do executável, onde ele
pode ser obtido sem custo.

## Desenvolvimento

```powershell
make build
make tests
```
