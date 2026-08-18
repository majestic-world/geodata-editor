# Geodata Editor

Editor nativo para arquivos de geodata Lineage II (`.l2j`, `.l2g` e
`_conv.dat`). A distribuição contém somente
`GeodataEditor.exe`; não inclui gerador, visualizador de geração, assistente de
terminal ou ferramentas auxiliares.

## Distribuição Windows

```powershell
make editor
```

O executável público será criado em `dist\GeodataEditor.exe`. O perfil de
release usa LTO e remove símbolos de depuração antes da cópia.

## Uso

```powershell
.\dist\GeodataEditor.exe --input .\Test\22_22.l2g `
  --client-root "C:\Lineage II" --map 22_22_Classic
```

Também pode abrir o executável sem argumentos e selecionar o cliente, o mapa e
a geodata na tela inicial. O editor mantém o formato de origem ao salvar e
cria uma cópia `.l2j.bak`, `.l2g.bak` ou `_conv.dat.bak`.

O histórico local do último cliente, mapa e arquivo fica em
`%APPDATA%\GeodataEditor\editor-history.ini`.

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
