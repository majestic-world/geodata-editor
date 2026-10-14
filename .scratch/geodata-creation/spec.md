# Criação de geodata: camadas manuais assistidas por colisão e geração automática por área

Status: ready-for-agent

## Problem Statement

Quem edita geodata Lineage II no editor só consegue alterar camadas que já existem: mudar NSWE, mudar altura, converter o tipo de bloco e restaurar. Quando o mapa tem um piso com colisão (ponte, andar de prédio, plataforma, trecho de chão) e a geodata não tem camada naquela altura, ou a coluna só tem a sentinela `NULL_HEIGHT`, não há como corrigir. As colunas `NULL_HEIGHT` nem aparecem na viewport e não podem ser clicadas. Clicar numa ponte sem geodata não seleciona nada. Também não há como aproveitar a geometria de colisão, já carregada do cliente, para corrigir alturas divergentes ou NSWE incoerente. O usuário precisa recorrer a ferramentas externas para preencher lacunas e melhorar a geodata.

## Solution

O editor passa a **criar** geodata dentro do arquivo aberto (`.l2j`, `.l2g` ou `_conv.dat`), em duas fases.

- **Fase 1: criação manual assistida pela colisão.**
  - No modo "Criar camada", o usuário clica numa superfície de colisão e o editor cria camadas na altura dessa superfície, nas células do pincel. Cada célula acompanha a geometria (rampas, escadas), e as novas camadas já ficam ligadas às vizinhas.
  - O inspector permite "Adicionar camada" com altura digitada (inclusive em colunas sem geodata), "Remover camada" e "Ajustar à colisão" nas camadas selecionadas.
  - Um filtro torna visíveis as células sem geodata.
- **Fase 2: geração automática por área.**
  - O usuário seleciona uma área e pede "Gerar a partir da colisão".
  - O editor amostra a colisão por célula, cria as camadas que faltam, ajusta alturas divergentes e recalcula o NSWE.
  - Antes de aplicar, uma prévia mostra o que vai mudar, separado por categoria. O usuário escolhe quais categorias aplicar, e tudo vira uma única operação de desfazer.

Todas as operações novas entram no undo/redo, no contador de blocos alterados e nos overlays incrementais, e são salvas no formato original do arquivo.

## User Stories

### Fundação: camadas no documento

1. Como editor de geodata, quero adicionar uma camada numa coluna que só tem `NULL_HEIGHT`, para preencher um ponto sem geodata.
2. Como editor de geodata, quero adicionar uma camada extra numa coluna que já tem camadas, para representar uma ponte ou um andar acima do chão.
3. Como editor de geodata, quero que a camada criada fique na posição ordenada por altura, para que o servidor encontre o piso correto.
4. Como editor de geodata, quero que a altura digitada seja normalizada para o passo de 8 unidades do L2J, como já acontece em "Editar altura", para não gravar valores inválidos.
5. Como editor de geodata, quero que uma camada criada a uma altura muito próxima de outra da mesma coluna seja rejeitada com uma mensagem clara, para não gerar camadas duplicadas.
6. Como editor de geodata, quero ser avisado quando a coluna chegar ao limite de camadas do formato, para não produzir um arquivo inválido.
7. Como editor de geodata, quero que adicionar uma camada numa célula de um bloco simple altere só essa célula, para não mexer nas outras 63.
8. Como editor de geodata, quero que o bloco seja promovido sozinho ao tipo necessário (complex ou multilayer), para não ter que converter manualmente antes.
9. Como editor de geodata, quero que a nova camada nasça aberta para as vizinhas compatíveis, com os bits espelhados nelas, para que a superfície criada já seja caminhável e coerente.
10. Como editor de geodata, quero remover uma camada selecionada, para apagar pisos que não existem no mapa.
11. Como editor de geodata, quero que remover a última camada de uma coluna deixe a coluna como `NULL_HEIGHT`, para manter o arquivo válido.
12. Como editor de geodata, quero que remover uma camada feche os bits das vizinhas que apontavam para ela, para não deixar ligações para um piso inexistente.
13. Como editor de geodata, quero que cada criação, remoção ou ajuste seja uma única entrada de desfazer/refazer, com rótulo em pt-BR, para reverter com um Ctrl+Z.
14. Como editor de geodata, quero que o contador de blocos alterados e "Restaurar bloco-base" funcionem com as camadas criadas, para continuar controlando o que mudou.
15. Como editor de geodata, quero salvar e reabrir o arquivo nos três formatos sem perder as camadas criadas, para usar o resultado no servidor.
16. Como editor de geodata, quero que blocos não editados continuem byte a byte iguais ao salvar, como já acontece hoje, para que um diff do arquivo mostre só o que eu mudei.
17. Como editor de `_conv.dat`, quero que o cabeçalho reflita a nova contagem de células, para que o arquivo continue válido.

### Fase 1: interface manual assistida

18. Como editor de geodata, quero que o combo de camadas e a tabela "Camadas da célula" se atualizem depois de criar ou remover camadas, para selecionar a camada nova sem recarregar o projeto.
19. Como editor de geodata, quero que, ao selecionar uma coluna sem a camada visível, o inspector ofereça "Adicionar camada" em vez de só dizer que ela não existe, para agir ali mesmo.
20. Como editor de geodata, quero um botão "Remover camada" junto da tabela de camadas da célula, para apagar a camada selecionada.
21. Como editor de geodata, quero ligar um modo "Criar camada" em que o clique atinge a geometria de colisão e não as camadas existentes, para escolher pisos que ainda não têm geodata.
22. Como editor de geodata, quero que, nesse modo, o clique numa ponte crie camadas na altura da ponte em todas as células do pincel, para preencher a área de uma vez.
23. Como editor de geodata, quero que cada célula do pincel use a superfície amostrada mais próxima da altura clicada, para que rampas e escadas sigam a geometria real.
24. Como editor de geodata, quero que as células do pincel sem superfície caminhável perto da altura clicada sejam ignoradas e contadas na mensagem de status, para saber o que não foi criado.
25. Como editor de geodata, quero que a amostragem descarte superfícies íngremes demais ou sem espaço livre suficiente acima, para não criar piso dentro de paredes ou sob tetos baixos.
26. Como editor de geodata, quero que a amostragem considere todas as categorias de colisão, mesmo as que estão ocultas na visualização, para que o resultado não dependa do que está na tela.
27. Como editor de geodata, quero "Ajustar à colisão" nas camadas selecionadas, para corrigir alturas que divergem da geometria.
28. Como editor de geodata, quero que "Ajustar à colisão" ignore camadas sem superfície dentro da tolerância e diga quantas foram ignoradas, para não mover camadas para pisos errados.
29. Como editor de geodata, quero um filtro "Mostrar células sem geodata" que desenhe as colunas `NULL_HEIGHT` com uma cor própria, para encontrar as lacunas.
30. Como editor de geodata, quero que as camadas criadas apareçam e possam ser clicadas imediatamente, sem recarregar a cena, para continuar editando.
31. Como editor de geodata, quero que as ações de criação fiquem bloqueadas enquanto o projeto carrega ou sem cliente e geodata carregados, como as edições atuais, para não editar um documento em troca.

### Fase 2: geração automática por área

32. Como editor de geodata, quero selecionar uma área retangular e pedir "Gerar a partir da colisão", para preencher muitas lacunas de uma vez.
33. Como editor de geodata, quero que a geração respeite o limite atual da seleção retangular, para manter o tempo de resposta previsível.
34. Como editor de geodata, quero que a geração rode sem travar a interface, para continuar navegando na viewport.
35. Como editor de geodata, quero uma prévia com contagens de camadas a criar, alturas a ajustar, NSWE a alterar e camadas sem superfície, para avaliar o impacto antes de aplicar.
36. Como editor de geodata, quero ver na viewport, com uma cor por categoria, onde cada mudança proposta acontece, para conferir visualmente.
37. Como editor de geodata, quero marcar e desmarcar as categorias "Criar faltantes", "Ajustar alturas" e "Recalcular NSWE", para aplicar só o que confio.
38. Como editor de geodata, quero que nada mude no documento até eu clicar "Aplicar", e que "Cancelar" descarte a prévia, para experimentar sem risco.
39. Como editor de geodata, quero que aplicar a geração seja uma única entrada de desfazer, para reverter tudo de uma vez.
40. Como editor de geodata, quero que o NSWE calculado só abra uma direção quando a vizinha tiver piso compatível dentro do degrau máximo e nenhuma parede bloquear a passagem, para que a passabilidade gerada seja realista.
41. Como editor de geodata, quero que o NSWE gerado seja simétrico entre vizinhas, para manter a mesma convenção da edição manual.
42. Como editor de geodata, quero que a geração nunca remova camadas, só aponte as que não têm superfície, para não perder geodata que o servidor possa usar.
43. Como editor de geodata, quero que a geração não altere blocos que já editei manualmente nesta sessão, a menos que eu peça, para não sobrescrever correções feitas à mão.
44. Como editor de geodata, quero que gerar de novo sobre uma área já gerada não proponha mudanças, para confiar que o resultado é estável.
45. Como editor de geodata, quero que o resultado gerado continue editável pelas ferramentas manuais, para refinar o que a geração não acertou.

### Calibração e confiabilidade

46. Como mantenedor, quero que os limites de amostragem (intervalo mínimo entre camadas, espaço livre acima do piso, inclinação máxima) sejam constantes nomeadas e calibradas com arquivos e cliente reais, para que a geração concorde com a geodata original.
47. Como mantenedor, quero um teste com dados reais que gere numa área, salve num arquivo temporário, reabra e confirme que o arquivo de origem não mudou, para validar o fluxo de ponta a ponta.

## Implementation Decisions

### Decisões do usuário

- Abordagem em duas fases: manual assistida pela colisão (Fase 1) e geração automática por área (Fase 2). A Fase 1 pode ser entregue sozinha.
- O escopo é só o arquivo de geodata já aberto. Não se cria arquivo novo para uma região sem geodata, e o formato de salvamento não muda.
- Seams de teste aprovados: API pública do documento, um único módulo novo de colisão/geração, o cache de overlays existente e o teste de integração real existente.

### Documento (módulo de edição L2J)

- Novas operações públicas no documento:
  - **Inserir camadas.** Recebe alvos com célula e altura e uma política de NSWE inicial.
    - Normaliza a altura com a mesma regra de "Editar altura".
    - Numa coluna cuja única camada é `NULL_HEIGHT`, substitui a sentinela: um bloco simple é promovido a complex e um complex continua complex.
    - Nos demais casos, promove o bloco a multilayer e insere a camada na ordem de altura.
    - Rejeita, informando no resultado, alvos fora da grade, alturas a menos do intervalo mínimo de uma camada existente e colunas que passariam do limite do formato (255 camadas em L2J/L2G).
  - **Remover camadas.** Remove a camada endereçada. Se era a última da coluna, ela vira `NULL_HEIGHT` com NSWE fechado, porque o formato não aceita coluna vazia. Também fecha os bits opostos nas vizinhas que apontavam para ela.
  - **Ajustar à colisão.** Recebe alturas-alvo já resolvidas pelo módulo de colisão e move as camadas dentro da tolerância. A ordem por altura é mantida, como já faz a edição de altura em multilayer.
  - **Aplicar geração.** Recebe um plano de geração e o conjunto de categorias escolhidas e aplica tudo como uma única operação.
- A política de NSWE inicial padrão é "aberto com vínculos". Depois de inserir, a mesma operação reaproveita a lógica atual de abrir para vizinhas compatíveis dentro do degrau máximo e espelhar os bits opostos.
- Cada operação acumula os blocos alterados num único conjunto "antes" e faz um único registro de undo. Com isso, contagem de blocos alterados, estado de alteração, fila de render e restaurar bloco funcionam sem caminhos novos.
- Os resultados de operação seguem o padrão dos resultados de edição atuais: contagens de células e blocos alterados, blocos promovidos e uma lista de rejeições em texto.
- Uma nova constante de intervalo mínimo entre camadas da mesma coluna fica no módulo de edição, ao lado do degrau máximo e do passo de altura.
- O salvamento não muda: blocos alterados são reescritos e blocos limpos são copiados dos bytes originais. O cabeçalho do `_conv.dat` já recalcula a contagem de células.

### Módulo novo de colisão e geração

- Um módulo puro, sem GPU e sem UI, construído sobre a geometria de colisão que já fica na CPU depois do carregamento.
- **Índice espacial:** triângulos agrupados por bloco L2J (256×256 grupos de índices). É montado no worker de carregamento do projeto, junto com os buffers de colisão, e publicado com a cena.
- **Amostragem por coluna:** para uma célula, lança um raio vertical no centro contra os triângulos do grupo e devolve as superfícies caminháveis, ordenadas por altura. Caminhável significa inclinação no máximo igual à máxima e espaço livre acima pelo menos igual ao mínimo. As alturas saem normalizadas para o passo L2J e agrupadas pelo intervalo mínimo entre camadas.
- **Picking por colisão:** recebe um raio em coordenadas de mundo e devolve a célula e a altura da primeira superfície atingida. Percorre a grade como o picking atual de camadas e reaproveita o recorte do raio aos limites do mapa.
- **Planejamento de geração:** função pura que recebe o documento, o índice, a área e as opções e devolve um plano. Para cada célula, casa as superfícies amostradas com as camadas existentes e classifica cada caso:
  - superfície sem camada → criar;
  - camada com diferença acima da tolerância → ajustar;
  - camada sem superfície → órfã, só relatada.

  Depois calcula o NSWE da área: uma direção abre quando a vizinha tem camada compatível dentro do degrau máximo e um segmento horizontal entre os centros, na faixa de altura do personagem, não cruza triângulo não caminhável.
  - Por padrão, blocos já alterados manualmente na sessão ficam fora do plano.
  - O plano nunca contém remoções.
- Todas as categorias de colisão (terreno, static mesh, BSP, blocking volume) contam como sólido, independentemente dos filtros de exibição.
- A correspondência célula ↔ mundo é a mesma do editor atual: o canto mínimo dos limites do mapa de colisão é o canto da região, a célula mede 16 unidades e a altura da geodata é o eixo vertical da colisão, na escala 1:1.
- Constantes nomeadas com valores provisórios, a calibrar com dados reais antes da Fase 2:

  | Constante | Valor provisório |
  |---|---|
  | Intervalo mínimo entre camadas | 32 |
  | Espaço livre acima do piso | 48 |
  | Inclinação máxima | 45° |

### Editor (UI)

- Novas ações do editor: criar camadas pela colisão, adicionar camada com altura, remover camada, ajustar à colisão, gerar por área, aplicar prévia e cancelar prévia. Todas passam pelo despacho central de ações e respeitam os bloqueios atuais (projeto carregando, contexto ausente, seleção oculta).
- O número máximo de camadas exibido (combo de camadas e tabela da célula) é recalculado depois de toda operação que muda a contagem de camadas. Hoje ele só é definido no carregamento.
- **Inspector, seção Passabilidade:**
  - a mensagem "A camada visível não existe nesta coluna." dá lugar aos controles de "Adicionar camada" (altura e botão);
  - "Remover camada" e "Ajustar à colisão" ficam junto da tabela de camadas da célula.
- **Modo "Criar camada":** é uma alternância. Com ela ligada, o clique simples usa o picking por colisão. As células do pincel usam a superfície amostrada mais próxima da altura clicada, dentro do intervalo mínimo. O status informa quantas células foram criadas e quantas rejeitadas.
- O filtro "Mostrar células sem geodata" entra nas opções de overlay e desenha as colunas `NULL_HEIGHT` com cor própria, pelo mesmo caminho de instâncias e chunks.
- **Geração (Fase 2):** parte da seleção retangular, roda o planejamento no worker e mostra a prévia como overlay temporário com cores por categoria, mais um resumo numérico e as checkboxes de categoria. "Aplicar" chama a operação de aplicar geração no documento; "Cancelar" descarta a prévia.
- Rótulos de undo e mensagens em pt-BR, como os atuais: "Criar camada", "Remover camada", "Ajustar à colisão", "Gerar geodata".

## Testing Decisions

- Um bom teste valida comportamento observável por um seam público: o estado do documento depois de uma operação, os bytes salvos e reabertos, as superfícies e o plano devolvidos para uma cena sintética, e os overlays incrementais iguais à reconstrução completa. Testes não verificam estruturas internas, ordem de chamadas, texto de mensagens ou valores padrão incidentais.

- **Seam 1: API pública do documento.** Testes unitários com documento em branco e bytes L2J sintéticos, no padrão dos testes atuais do módulo de edição:
  - substituir `NULL_HEIGHT` em blocos simple e complex (no simple só a célula alvo muda);
  - inserir ordenado em multilayer;
  - rejeitar pelo intervalo mínimo e pelo limite de camadas;
  - vínculos NSWE abertos e espelhados na inserção, e fechados na remoção;
  - remover a última camada gera `NULL_HEIGHT`;
  - cada operação é uma única entrada de undo/redo, e desfazer restaura exatamente o estado anterior;
  - aplicar geração com subconjuntos de categorias;
  - round trip salvar/reabrir em `.l2j`, `.l2g` e `_conv.dat`, com cabeçalho do `_conv.dat` correto e blocos não editados byte a byte iguais.
- **Seam 2: módulo novo de colisão e geração.** Testes unitários com mapa de colisão sintético (triângulos montados à mão e limites fixos, no padrão dos testes atuais de picking de camadas):
  - chão mais ponte gera duas superfícies;
  - rampa íngreme é descartada;
  - teto baixo é descartado pelo espaço livre;
  - triângulo na fronteira entre grupos do índice;
  - picking por colisão atinge a superfície mais próxima e devolve a célula e a altura corretas;
  - o planejamento classifica criar, ajustar e órfã;
  - parede entre células fecha o NSWE e escada dentro do degrau abre;
  - blocos já alterados ficam fora por padrão;
  - idempotência: planejar sobre o resultado aplicado não propõe mudanças.
- **Seam 3: seams existentes reaproveitados.**
  - Cache de overlays: comparar a atualização incremental com a reconstrução completa depois de inserir e remover camadas e com o filtro de células sem geodata ligado. O modelo são os testes atuais de chunks.
  - Integração com dados reais (ignorada, variáveis de ambiente do L2J e do cliente): gerar numa área, salvar em arquivo temporário, reabrir, validar e confirmar que o arquivo de origem não mudou. O modelo é o teste de integração real atual.
- **UI** (inspector, modo "Criar camada", prévia): sem testes automatizados, por decisão do usuário. A validação é por smoke manual no app real: criar camada numa ponte, adicionar e remover camada pelo inspector, ajustar à colisão, gerar numa área pequena, conferir a prévia, aplicar, desfazer e salvar.

## Out of Scope

- Criar arquivo de geodata novo para uma região sem geodata, ou converter entre formatos.
- Remover camadas automaticamente na geração. Camadas órfãs só são relatadas.
- Otimizar o tipo de bloco automaticamente (colapsar para simple ou complex) além das conversões manuais que já existem.
- Prévia da superfície de colisão ao passar o mouse.
- Mudanças no modo de visualização texturizada.
- Testes automatizados de UI.

## Further Notes

- **Dados reais para a implementação:** `C:\Users\Dev\Desktop\geodata` tem 166 arquivos `.l2j` reais (regiões `16_10` em diante). Nos testes ignorados e na calibração, aponte `GEODATA_EDITOR_L2J` para um arquivo dessa pasta (por exemplo, `17_22.l2j`). Ainda não se sabe como as lacunas aparecem nesses arquivos: sentinela `NULL_HEIGHT`, camada faltando em multilayer ou célula na altura do terreno com NSWE fechado. A calibração das constantes deve medir isso, junto com a distribuição dos intervalos entre camadas consecutivas e a concordância do gerador com a geodata original numa área de referência. Para a amostragem de colisão ainda é preciso apontar `GEODATA_EDITOR_CLIENT` para um cliente.
- **Risco:** GeoEngines do lado do servidor podem ter limite de camadas por coluna menor que 255 ou exigir um intervalo mínimo próprio. Vale validar um arquivo gerado num servidor de teste.
- **Risco:** o NSWE automático depende da qualidade da colisão (BSP, blocking volumes). Por isso é uma categoria opcional na prévia.
- **Risco:** ainda falta medir memória e tempo do índice espacial numa região densa. Ele é montado no worker de carregamento.
- **Ordem sugerida:**
  1. operações do documento;
  2. contagem dinâmica de camadas e inspector;
  3. filtro de células sem geodata;
  4. índice e amostragem (em paralelo com 1–3);
  5. modo "Criar camada";
  6. ajustar à colisão;
  7. calibração com dados reais;
  8. planejamento de geração;
  9. prévia e aplicação;
  10. README.
