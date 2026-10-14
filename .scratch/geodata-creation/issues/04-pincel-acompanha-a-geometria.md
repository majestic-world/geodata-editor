# 04: Pincel do modo "Criar camada" acompanha a geometria

**What to build:** Spec: histórias 22–25.

No modo "Criar camada", o clique cria camadas em todas as células do pincel, não só na célula atingida. Para cada célula, o módulo de colisão faz a amostragem por coluna: um raio vertical no centro da célula devolve as superfícies caminháveis ordenadas por altura (inclinação no máximo a máxima, provisória 45°; espaço livre acima pelo menos o mínimo, provisório 48), normalizadas para o passo L2J e agrupadas pelo intervalo mínimo entre camadas. Cada célula usa a superfície mais próxima da altura clicada, dentro do intervalo mínimo, de modo que rampas e escadas sigam a geometria. As células sem superfície próxima são ignoradas, e o status informa quantas foram criadas e quantas rejeitadas. Tudo é uma única entrada de undo.

**Blocked by:** 03 — Criar camada clicando na colisão (célula única)

**Status:** ready-for-agent

- [ ] Clicar numa rampa com pincel 4×4 cria camadas cujas alturas acompanham a rampa, célula a célula
- [ ] As células do pincel sem superfície caminhável perto da altura clicada não recebem camada e entram na contagem de rejeitadas no status
- [ ] Testes unitários da amostragem com cenas sintéticas: chão mais ponte gera duas superfícies; rampa íngreme é descartada; teto baixo é descartado pelo espaço livre
- [ ] As constantes de inclinação máxima e espaço livre são nomeadas e ficam junto do módulo de colisão
- [ ] A criação do pincel inteiro é uma única entrada de undo
