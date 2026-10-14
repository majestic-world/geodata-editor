# 03: Criar camada clicando na colisão (célula única)

**What to build:** Spec: histórias 21, 26, 30, 31; seção "Módulo novo de colisão e geração".

O editor liga o modo "Criar camada" e clica numa superfície de colisão, por exemplo uma ponte sem geodata. O clique passa a acertar a geometria de colisão em vez das camadas existentes. É criada uma camada na célula atingida, na altura atingida (normalizada), com as mesmas regras do ticket 01. Isso introduz o módulo novo de colisão, puro e sem GPU/UI: o índice espacial de triângulos por bloco L2J (256×256 grupos), montado no worker de carregamento junto com os buffers de colisão e publicado com a cena, e o picking por raio contra a colisão, que percorre a grade como o picking atual de camadas, reaproveita o recorte do raio aos limites do mapa e devolve célula e altura. Todas as categorias de colisão contam, independentemente dos filtros de exibição.

**Blocked by:** 01 — Adicionar camada com altura digitada

**Status:** ready-for-agent

- [ ] Com o modo "Criar camada" ligado, clicar numa ponte sem geodata cria uma camada na célula e na altura da ponte, visível e clicável imediatamente
- [ ] Com o modo desligado, o clique continua selecionando camadas existentes como hoje
- [ ] Ocultar uma categoria de colisão na visualização não muda o resultado do picking por colisão
- [ ] Clique sem colisão atingida não altera o documento e informa no status
- [ ] O índice espacial é montado em segundo plano durante o carregamento, sem travar a interface; tempo e memória medidos numa região real ficam registrados no ticket
- [ ] Testes unitários com mapa de colisão sintético: o picking devolve a superfície mais próxima com a célula e a altura corretas, inclusive com triângulo na fronteira entre grupos do índice
- [ ] Uma única entrada de undo desfaz a criação
