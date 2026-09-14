# Geodata Editor

Editor nativo para arquivos de geodata Lineage II (`.l2j`, `.l2g` e
`_conv.dat`).

## Distribuição Windows

```powershell
make build
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

O carregamento do projeto e das texturas ocorre em segundo plano. A cena atual
permanece visível até a substituição estar pronta; a edição fica bloqueada durante
a troca de projeto. A primeira ativação de texturas prepara os materiais, e as
ativações seguintes reutilizam a cena carregada.

O contador de blocos alterados é incremental. Os overlays são atualizados em
chunks de 16×16 blocos, com buffers reutilizados; ícones NSWE ocultos só são
atualizados quando voltam a ficar visíveis.

Para verificar o carregamento completo com um cliente local e uma GPU:

```powershell
$env:GEODATA_EDITOR_CLIENT = "C:\caminho\cliente"
$env:GEODATA_EDITOR_L2J = "C:\caminho\22_22.l2j"
cargo test --release real_worker_prepares_matching_document_collision_overlays_and_textures -- --ignored --nocapture
```

Esse teste lê a geodata sem salvá-la, mas atualiza o histórico do editor.
Defina `APPDATA` para uma pasta temporária se quiser isolar esse histórico.
