# 09: Recalcular NSWE na geração

**What to build:** Spec: histórias 40–41.

A prévia de geração ganha a categoria "Recalcular NSWE". Para as células da área, uma direção abre quando a vizinha tem camada compatível dentro do degrau máximo e um segmento horizontal entre os centros, na faixa de altura do personagem, não cruza triângulo de colisão não caminhável (parede). Os bits ficam simétricos entre vizinhas, na mesma convenção da edição manual. As mudanças de NSWE aparecem no resumo, com cor própria na prévia, e são aplicadas na mesma operação única de undo das demais categorias marcadas.

**Blocked by:** 08 — Gerar a partir da colisão: criar faltantes e ajustar alturas

**Status:** ready-for-agent

- [ ] Numa cena com parede entre duas células, a direção entre elas fica fechada nos dois lados
- [ ] Numa escada com degraus dentro do degrau máximo, as direções ao longo da escada ficam abertas nos dois lados
- [ ] Com a categoria desmarcada, o NSWE não muda; com ela marcada, a mudança entra na mesma entrada de undo das outras categorias
- [ ] A geração continua idempotente com a categoria marcada
- [ ] Testes unitários com cenas sintéticas cobrem parede, escada e simetria
