# Visualização 3D texturizada (static mesh + texturas) — plano de viabilidade

Status: **implementado (v1)** — Fases 1–4 completas, Fase 5 na variante v1 (camada base do terreno, sem blend), verificado contra o client real `Lucera2Classic` (mapa `17_22_Classic`).

## Contexto da pergunta

Duas perguntas foram feitas, comparando com `C:\Workspace\Tauri\unreal-tools-rs`:

1. Dá pra recuperar as texturas do client e subir as necessárias em memória?
2. Dá pra usar essas texturas pra recriar o mapa com static mesh, como uma visualização estilo Unreal Engine 2.5, em vez da representação atual?

**Resposta curta: sim para as duas, mas são dois problemas de tamanho muito diferente.** A pergunta 1 é uma extensão pontual do leitor de pacotes que já existe. A pergunta 2 é uma reescrita relevante do pipeline de render — viável, mas é o grosso do esforço deste plano.

## 1. Estado atual do `geodata-editor` (evidências no código)

O `geodata-editor` já tem um leitor de pacotes Unreal completo em `src/unreal.rs` (`Archive`, `Object`, `Reader`), reaproveitado para `.unr` (mapas), `.usx` (static meshes) e `.utx` (texturas) — inclusive a mesma descriptografia L2 para os três (`decrypt()`, `unreal.rs:339`). Isso já resolve autenticação/acesso ao client; o gap é o que é **feito** com os dados depois de descriptografados:

| Dado | O que já é lido | O que é descartado hoje |
|---|---|---|
| `TerrainInfo` (terreno) | Só a heightmap (`TerrainMap`, formato G16) via `Texture::heights()` (`unreal.rs:909`) | A propriedade `Layers` (texturas de grama/terra/rocha com blend por alpha) nunca é lida pela struct `Terrain` (`unreal.rs:781`) — mas o parser genérico de propriedades **já sabe decodificar** o struct `TerrainLayer` (`unreal.rs:1646`), só não é chamado |
| `StaticMesh` (`.usx`) | Geometria completa: vértices, normais, streams de UV, superfícies (`StaticMesh::read`, `unreal.rs:952`) | As UVs são lidas e jogadas fora (`let _uvs = …`, `unreal.rs:990`); `collision_mesh()` (`unreal.rs:1015`) descarta toda superfície cujo material não tenha `EnableCollision=true` — ou seja, meshes decorativos (sem colisão) nem chegam a aparecer |
| Atores (`StaticMeshActor`, …) | Só os que têm `collide_actors && block_actors && block_players` (`unreal.rs:228`, dentro de `load_map`) | Todo ator puramente decorativo (a maioria da vegetação/props) é descartado **antes** de chegar no static mesh |
| BSP (`Model`/`BspSurface`) | Geometria de nós/superfícies do brush do level (`unreal.rs:1114`) | O primeiro `reader.index()` de cada superfície — que é a referência de material/textura — é lido e jogado fora sem nome de campo (`unreal.rs:1148`) |
| `Texture` (`.utx`) | Só o mip 0 quando o formato é um dos 5 suportados (`3\|5\|7\|8\|10`), e só é *usado* quando `format == 10` (G16, heightmap) via `heights()` | Para qualquer textura de cor real (DXT1/DXT3/DXT5/RGBA8 = formatos 3/5/7/8), os bytes do mip são lidos só pra saber quanto pular (`_discarded`, `unreal.rs:899`) — nunca viram pixels |

Ou seja: **o parser já entende a maior parte do formato binário necessário** (UVs, materiais, referências de textura, mips) — ele só não guarda essa informação, porque hoje o único objetivo do loader é gerar uma malha de colisão para picking/edição de passabilidade.

No lado de render (`src/editor_view.rs`), o vértice usado em toda a cena 3D é:

```rust
struct Vertex { position: [f32; 3], color: [f32; 4], normal: [f32; 3] }
```

Sem UV. `CollisionMeshes::new` (`editor_view.rs:3443`) pinta cada categoria de geometria com uma cor fixa (terreno cinza, static mesh salmão, BSP amarelo-claro, blocking volume laranja) — é deliberadamente um visualizador de **passabilidade/colisão**, não uma reconstrução visual do mapa. O único ponto do código que já sobe uma textura pra GPU é o atlas 2D dos ícones NSWE (`create_nswe_icon_resources`, `editor_view.rs:4005`), que é um bom modelo de bind group (textura + sampler) mas não foi pensado pra dezenas/centenas de materiais diferentes.

## 2. O que o `unreal-tools-rs` já resolve

`src-tauri/src/utx.rs` + `texture_engine.rs` têm um leitor/escritor de UTX maduro, testado contra pacotes reais de L2:

- Parsing genérico de mip array (contagem de mips, tamanho comprimido, largura/altura por mip) — o `geodata-editor` só tem uma heurística de mip único usada para heightmap.
- Decodificação real de pixels: DXT1/DXT3/DXT5 (`decode_dxt`, `dxt_palette`, `utx.rs:4421`) e conversão BGRA→RGBA (`bgra_to_rgba`), cobrindo os formatos `UtxFormat` (P8, RGBA7, RGB16, DXT1, RGB8, RGBA8, NODATA, DXT3, DXT5, L8, G16).
- A mesma descriptografia L2 (`decrypt_raw`/`compute_key_121`), então não há nada novo a resolver em criptografia.

**Importante:** os dois projetos são binários Rust separados, sem crate compartilhada — `unreal-tools-rs` não pode virar uma dependência do `geodata-editor` (é um app Tauri, com `serde`/tipos de UI acoplados nas structs de export). O reaproveitamento real é **portar** um conjunto pequeno e bem isolado de funções puras (parsing de mip array + decodificação DXT) para dentro de `unreal.rs`, adaptando para o `Reader`/`AppError` que já existem aqui. Não é copy-paste do arquivo inteiro — é extrair a lógica de decodificação, não a camada de import/export/edição de texturas (irrelevante para um visualizador).

## 3. Plano faseado

Recomendação de escopo: implementar como um **modo adicional** — "Visualização texturizada" — ativável ao lado do modo atual de passabilidade (que continua sendo o padrão), não como substituição. O propósito central do app é editar geodata/passabilidade; a textura é um apoio de orientação espacial, e o modo debug colorido continua sendo necessário para o trabalho de edição em si.

### Fase 1 — Decodificação de textura (isolada, sem tocar em render)
- Generalizar `Texture::read` para ler o array de mips completo (contagem + por mip: tamanho, largura, altura, bytes), não só a heurística de mip único.
- Portar `decode_dxt1/3/5` + conversão para RGBA8 do `unreal-tools-rs`. Adicionar RGBA8/BGRA8 direto (já trivial) e sinalizar formatos não cobertos (P8/RGB8/RGB16/RGBA7) como "não suportado ainda" em vez de falhar — a maioria dos assets de terreno/prop usa DXT1/DXT3/RGBA8.
- Critério de pronto: teste unitário decodificando uma textura real do client (arquivo local, como já existe o padrão `GEODATA_EDITOR_L2J` ignorado em CI) e comparando dimensões/alguns pixels.
- Tamanho: M.

### Fase 2 — Capturar referências de material (sem decodificar ainda)
- `BspSurface`: nomear e guardar o `reader.index()` hoje descartado como `material_index` (`unreal.rs:1148`).
- `StaticMesh`: em vez de só extrair `EnableCollision` de `array_maps("Materials")`, também extrair o índice de objeto do material (`Properties::index("Material")` na mesma struct) por superfície; manter as UVs lidas por vértice em vez de descartar.
- `Terrain`: ler `props.array_maps("Layers")` e, por camada, capturar `Texture` (índice) + escala/pan — a decodificação de struct `TerrainLayer` já existe (`unreal.rs:1646`), só falta o `Terrain::read` consumir.
- Resolver os índices de material/textura para nome de exportação, igual já é feito para static mesh/terrain hoje (`object_ref`/`texture_ref`).
- Critério de pronto: logar (modo verbose) o material resolvido por superfície de um mapa real e conferir contra o client.
- Tamanho: M.

### Fase 3 — Pipeline de render texturizado
- Novo formato de vértice com UV: `{ position, normal, uv }`, separado do `Vertex` de debug atual (que continua existindo para o modo passabilidade).
- Novo par de shaders WGSL (vertex+fragment) com sampling de textura, reaproveitando o padrão de bind group já usado no atlas NSWE (`create_nswe_icon_resources`) como referência — mas criado por material, não um atlas único fixo.
- Agrupar desenho por material (batch por bind group), já que wgpu 0.19 não tem bindless texture arrays maduro nesta versão — isso é o modelo clássico de forward renderer por material, adequado ao volume de uma região por vez.
- Cache de textura decodificada por índice de objeto (mesmo padrão de `HashMap` já usado em `PackageLoader.archives`), para nunca decodificar a mesma textura duas vezes nem subir a GPU texturas não referenciadas pela região carregada.
- Critério de pronto: um static mesh isolado (ex.: uma pedra ou árvore) aparece texturizado corretamente na viewport, com toggle para ligar/desligar.
- Tamanho: L.

### Fase 4 — Geometria visual completa (não só colisão)
- Novo caminho de carregamento paralelo ao de colisão: quando o modo texturizado está ativo, carregar **todas** as superfícies de `StaticMesh` (não só as com `EnableCollision`) e **todos** os atores (não só os filtrados por `collide_actors/block_actors/block_players`), preservando por-superfície o material e a UV.
- BSP: usar o `material_index` capturado na Fase 2 para agrupar triângulos por material ao montar o mesh do brush, e ligar U/V real a partir de `base_index`/`normal_index`/`u_index`/`v_index` (hoje descartados em `Model::mesh`, `unreal.rs:1262`) em vez de UV planar improvisado.
- Critério de pronto: uma região pequena renderiza com densidade visual comparável ao client (vegetação, props, arquitetura), mantendo o modo passabilidade acessível por toggle.
- Tamanho: L.

### Fase 5 — Terreno multi-camada (a parte mais cara)
- Terreno UE2/L2 usa 1 camada base + até ~8 camadas com alpha blending (grama/terra/rocha/neve etc.), cada uma com sua textura e (possivelmente) máscara de peso por vértice/quad.
- **v1 recomendado:** renderizar só a camada base (primeiro item de `Layers`) esticada pela UV/escala do terreno — visualmente já é uma melhora enorme sobre o cinza sólido atual, com esforço bem menor.
- **v2 (opcional, depois):** shader de blend com N texturas + N máscaras de alpha (sampler array pequeno, ex. até 4-8 camadas), replicando o splatting do client.
- Critério de pronto v1: terreno aparece com a textura base do bioma correto, sem preto/roxo (fallback óbvio quando faltar dado).
- Tamanho: v1 = M, v2 = L (não bloqueante para o resto do plano).

### Fase 6 — Orçamento de memória/performance
- Limitar decodificação/upload a texturas efetivamente referenciadas pela região carregada (já é o comportamento hoje para pacotes — `loaded_package_count`), nunca decodificar `Textures.utx`/`SysTextures.utx` inteiros.
- Cap de tamanho de textura carregada (ex.: usar um mip intermediário em vez do mip 0 para texturas muito grandes) — o mip array já dá essa opção de graça a partir da Fase 1.
- Medir tempo de carregamento e VRAM numa região densa antes de liberar por padrão; manter o modo passabilidade como fallback caso o texturizado seja pesado demais numa máquina modesta.
- Tamanho: S, mas deve rodar antes de fechar a feature, não depois.

## 4. Riscos e questões em aberto

- **Formatos não cobertos por nenhum dos dois códigos ainda** (P8 com paleta, RGB8, RGB16, RGBA7): raros em terreno/props principais, mas podem aparecer em algum client específico; tratar como "sem preview, mesh sem textura" em vez de falhar o carregamento do mapa inteiro.
- **wgpu 0.19 sem bindless real**: o agrupamento por material funciona bem até uma ou duas centenas de materiais únicos por região; se um mapa tiver muito mais que isso, pode exigir atlas dinâmico — não é esperado ser um problema para uma região de cada vez, mas vale medir na Fase 3.
- **Divergência entre os dois parsers Unreal**: como não há crate compartilhada, o `unreal.rs` do geodata-editor e o `utx.rs`/`Package` do unreal-tools-rs vão continuar evoluindo separados. Se no futuro os dois projetos precisarem manter formato em sincronia, vale considerar extrair um crate `unreal-format` compartilhado — fora do escopo deste plano, só registrando a decisão consciente de não fazer isso agora.
- **Terreno multi-camada fiel ao client é o item de maior incerteza de esforço** deste plano; por isso está isolado na Fase 5 com uma v1 deliberadamente mais simples.

## 5. Resumo da resposta às duas perguntas

1. **Recuperar texturas do client e subir as necessárias em memória**: sim, é a Fase 1 (decodificação, portando lógica já validada no `unreal-tools-rs`) + parte da Fase 6 (só as necessárias, com cache). Risco baixo, escopo bem contido.
2. **Recriar o mapa com static mesh e texturas como uma visualização Unreal 2.5**: sim, é tecnicamente viável e o caminho está claro (Fases 2–5), mas é a reescrita real: novo formato de vértice, novos shaders, novo agrupamento de geometria por material, e uma decisão consciente de manter isso como modo adicional em vez de substituir a visualização de passabilidade que é a razão de existir do editor.

## 6. Relatório de implementação (v1)

Implementado em `src/unreal.rs` e `src/editor_view.rs`, verificado contra o client real `Lucera2Classic` (mapa `17_22_Classic`, 28 pacotes de contexto).

**Fase 1 — decodificação**: `Texture::read` agora lê o layout real do array de mips (contagem em 1 byte, `TLazyArray` com offset+contagem compact-index, dimensões por mip) em vez da heurística antiga de mip único; `decode_dxt`/`dxt_palette`/`bgra_to_rgba` portados do `unreal-tools-rs` cobrem DXT1/DXT3/DXT5/RGBA8. Teste `unreal::tests::decodes_real_client_textures_and_visual_scene_material_refs` (ignorado, requer `GEODATA_EDITOR_CLIENT`) decodifica 7 texturas reais do mapa (1024×1024 DXT1) e valida `rgba.len() == width*height*4`.

**Fase 2 — captura de materiais**: `BspSurface.material_index` (antes descartado), `StaticMesh.materials: Vec<MaterialSlot>` (`enable_collision` + `material_index`, mais `uvs` do primeiro UV stream), `Terrain.layers: Vec<TerrainLayer>` (`texture_index` + `u_scale`/`v_scale`, via novo `Properties::all_struct_maps` para arrays fixos de struct). Confirmado no client real: `Materials[i].Material` existe em 100% dos slots inspecionados; `TerrainInfo.Layers` tem 8 entradas com `UScale`/`VScale` reais (1 ou 2).

**Fases 3–4 — cena visual**: novos tipos `VisualTexture`/`VisualMesh`/`VisualBatch`/`VisualScene` (`unreal.rs`) e `PackageLoader::load_visual_scene`, que carrega **todas** as superfícies de `StaticMesh` (não só `EnableCollision`) de **todos** os atores (sem o filtro `collide_actors/block_actors/block_players`), mais BSP com UV real via `Base`/`TextureU`/`TextureV` (`points[base_index]`, `vectors[u_index]`, `vectors[v_index]`), agrupados por identidade do buffer de pixels decodado (dedup automático entre materiais que compartilham textura).

**Fase 5 v1 — terreno**: `terrain_visual_mesh` usa só a primeira camada (`Layers[0]`), UV = coordenada de grade dividida por `UScale`/`VScale`. Sem blend entre camadas (v2, não implementado).

**Render**: novo par de shaders WGSL (`TEXTURED_SHADER`), vértice `{position, normal, uv}`, pipeline sem cull (`cull_mode: None` — winding não é normalizado no caminho visual, ao contrário do `SourceMap::add_mesh` de colisão), bind group de material por lote (textura `Rgba8UnormSrgb` + sampler `Repeat`), fallback cinza `(170,170,170,255)` para materiais não resolvidos (Shader/Combiner/FinalBlend — não modelados). Toggle "Textura" na toolbar (`EditorUi.textured_view`), carregado sob demanda (`enable_textured_view`/`apply_visual_scene`), cache invalidado ao trocar de projeto em `open_project`.

**Verificação real** (client `Lucera2Classic`, mapa `17_22_Classic`):
- `cargo test`: 50 passed, 2 ignored (os dois testes que exigem client/L2J reais) — sem regressão.
- App real lançado e capturado em screenshot: tela de boas-vindas → "Carregar projeto" → "Projeto carregado: 17_22_Classic com 28 pacotes de contexto." (visualização de passabilidade original intacta) → toggle "Textura" → **"Visualização texturizada carregada: 45 lote(s)."**, com o terreno renderizado com a textura de grama real do client (em vez do cinza sólido).

**Correção aplicada após o relato de "visualização quebrada"**: o gap acima (ruído/aliasing) não era só uma lacuna de qualidade — sem mipmaps, a textura de 256×256 repetida ~255× no tile do terreno virava ruído por pixel a qualquer distância de câmera, tornando a visualização ilegível. Corrigido em `create_material_bind_group` (`editor_view.rs`): gera uma cadeia completa de mips por box filter 2×2 (`downsample_rgba`) até 1×1 e sobe todos os níveis via `queue.write_texture` por mip, com `mip_level_count` real e `mipmap_filter: Linear` no sampler. `cargo test` seguiu verde (50 passed, 2 ignored) após a correção. Reverificação visual via screenshot automatizado não foi repetida nesta rodada porque a automação de clique (SetForegroundWindow + SetCursorPos) se mostrou não confiável com o desktop do usuário em uso ativo (roubo de foco falhou silenciosamente e a captura pegou outra janela) — risco de interferir na sessão do usuário, então a automação foi interrompida. A correção é padrão/bem estabelecida (mipmapping para minimização de textura) e a suíte de testes automatizados não cobre pixels renderizados; recomenda-se conferência visual manual pelo usuário.
