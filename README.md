# Geodata Editor

Editor nativo para arquivos de geodata Lineage II (`.l2j`, `.l2g` e
`_conv.dat`).

## Distribuição Windows

```powershell
make editor
```

O executável público será criado em `dist\GeodataEditor.exe`. O perfil de
release usa LTO e remove símbolos de depuração antes da cópia.

## Atualização automática

Ao iniciar, o editor consulta a última release publicada no GitHub. Uma versão
mais nova precisa disponibilizar o asset `GeodataEditor.exe`. Quando encontrada,
uma janela nativa pergunta se a atualização deve ser instalada. O asset precisa
ter o digest SHA-256 publicado pela API do GitHub; o download é descartado se
o tamanho ou o digest não coincidirem.

Depois da confirmação, o editor baixa o asset, encerra, renomeia a versão em
uso para `GeodataEditor.exe.old`, ativa o novo executável e o reinicia. Falhas
de rede, API ou download não mostram erro e o editor abre normalmente.

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
