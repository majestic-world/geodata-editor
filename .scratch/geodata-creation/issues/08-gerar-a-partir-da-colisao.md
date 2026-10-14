# 08: Gerar a partir da colisão: criar faltantes e ajustar alturas

**What to build:** Spec: histórias 32–39, 42–45.

O editor seleciona uma área retangular (até o limite atual da seleção, 65.536 células) e usa "Gerar a partir da colisão". O planejamento de geração, uma função pura do módulo de colisão, roda no worker sem travar a interface. Para cada célula, ele casa as superfícies amostradas com as camadas existentes e classifica cada caso: superfície sem camada → criar; camada com diferença acima da tolerância → ajustar; camada sem superfície → órfã, só relatada. Blocos já alterados manualmente na sessão ficam fora do plano, a menos que uma opção explícita os inclua. O plano nunca contém remoções.

A prévia mostra um resumo numérico por categoria, uma cor por categoria num overlay temporário na viewport e as checkboxes "Criar faltantes" e "Ajustar alturas". Nada muda no documento até "Aplicar". "Aplicar" chama a operação de aplicar geração no documento com as categorias marcadas, como uma única entrada de undo com rótulo "Gerar geodata". As camadas criadas nascem "abertas com vínculos". "Cancelar" descarta a prévia.

**Blocked by:** 05 — Ajustar à colisão; 07 — Calibrar constantes com dados reais

**Status:** ready-for-agent

- [ ] Gerar sobre uma área com uma ponte sem geodata mostra a criação dessas camadas na prévia, com contagem e cor próprias
- [ ] Enquanto o plano é calculado, a viewport continua navegável
- [ ] O documento só muda depois de "Aplicar"; "Cancelar" descarta a prévia e o overlay temporário
- [ ] Desmarcar uma categoria impede que ela seja aplicada
- [ ] "Aplicar" gera uma única entrada de undo, e desfazer restaura exatamente o estado anterior
- [ ] Camadas órfãs aparecem só no resumo e na prévia; nenhuma camada é removida
- [ ] Blocos já alterados manualmente na sessão ficam fora do plano por padrão
- [ ] Gerar de novo sobre o resultado aplicado não propõe mudanças (idempotência)
- [ ] As camadas geradas continuam editáveis pelas ferramentas manuais
- [ ] Testes unitários com cena sintética cobrem a classificação criar/ajustar/órfã, a exclusão de blocos alterados, a idempotência e a aplicação de subconjuntos de categorias pela API do documento
