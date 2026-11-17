# Validação REST #5 — roteiro para o dono

Estado: **pendente de observação no serviço**. O agente não acessou conta, WebView, perfil ou credenciais. Este roteiro não autoriza escrita: o dono deve primeiro autorizar expressamente um envio HTML em chat exclusivamente próprio e confirmar que o tenant permite o teste. Sem isso, apenas observação de leitura. Não alterar o padrão de envio do Fast365 com base em fixtures.

## Preparação e registro seguro

1. O dono opera em seu próprio Windows, fora do ambiente do agente, em sessão autorizada. Escolha um chat exclusivamente próprio com histórico suficiente para várias páginas; não envolva terceiros. Interrompa diante de política restritiva, challenge, rate limit ou resultado inesperado; não tente contorná-los.
2. Registre apenas data, versão/commit, ambiente em termos genéricos, operação, nomes de campos, presença/ausência dos headers, status HTTP e contagens. Registre conclusões como observação limitada à conta, tenant, endpoint e momento. Não salve HAR, capturas brutas, bodies, conteúdo de mensagens, IDs de pessoas/conversas/mensagens, tokens, cookies, valores de autorização, URLs completas ou assinadas, queries/cursors. Inspecione localmente e transcreva uma tabela sanitizada; revise antes de compartilhar.
3. O navegador pode acrescentar ou restringir headers automaticamente. Marque como inconclusivo qualquer comparação em que não seja possível confirmar a presença/ausência real no request efetivo. Não exponha valores de sessão ao agente.

## Headers do agregador (§3.3)

Para uma operação **GET de listagem** já conhecida, mantenha endpoint, parâmetros, sessão, demais headers e condições constantes. Registre um controle bem-sucedido, depois uma única variação sem um header por vez e, se seguro, repita o controle. Compare status e shape/contagem, sem conteúdo. Teste separadamente `X-MS-Client-Version`, `X-MS-Client-Type`, `X-MS-Client-Env`, `X-MS-Session-Id`, `X-MS-Correlation-Id`, `client-request-id`, `X-MS-Scenario-Id`, `X-MS-User-Type`, `User-Agent` e presença de cookies; se algum não estiver no controle, marque não observado. Não force remoção de cookie ou autorização se isso requer exportar segredos ou alterar configuração de segurança; marque inconclusivo. A comparação da autorização só é admissível com identidade própria e sem divulgação de credenciais; ausência pode produzir 401 mas não identifica audience nem prova requisito universal. Não modifique múltiplos headers juntos, e não teste headers em POST.

Modelo por linha: `operação | header (nome apenas) | controle presente? | controle status/shape | variante ausente confirmada? | variante status/shape | repetição controle | conclusão e limites`. Se controle falhar, resposta variar ou a remoção não for observável, resultado inconclusivo. Uma diferença isolada sugere necessidade nas condições testadas, não em todos os tenants; sucesso sem header demonstra apenas que ele não foi necessário naquela operação e sessão.

## Formato de envio

Somente após autorização expressa para esta escrita, use o cliente oficial no chat próprio e envie **uma única** mensagem inofensiva sintética formatada. Observe localmente a requisição correspondente: método, família de endpoint, nomes `messagetype` e `contenttype` e seus valores não sensíveis, existência de `content` e `clientmessageid` (sem registrar conteúdo ou ID). Anote status e confirmação visual sem texto nem captura bruta. Não repetir automaticamente, inclusive em timeout; verifique visualmente o resultado antes de qualquer nova ação. Essa observação comprova o que o cliente oficial enviou, não que o Fast365 aceite ou envie tal formato. Nenhum POST manual alternativo ou mudança no default é autorizado por este roteiro. Confirmar envio por outro cliente exige autorização específica e plano separado.

## Histórico e paginação

Observe requisições de leitura do chat próprio enquanto carrega pelo menos três páginas, se existirem. Anote para cada página número ordinal, status, número de itens, nomes dos campos de mensagem efetivamente presentes e presença/tipo estrutural de `_metadata.backwardLink` e `syncState` (por exemplo, string/objeto; nunca seu valor ou URL). Observe localmente se a próxima solicitação usa o valor de continuação anterior, sem registrá-lo; anote apenas sim/não/inconclusivo e qual campo. Verifique fim do histórico e, sem registrar IDs, compare localmente interseções entre páginas: quantos itens repetidos e quantos novos, sem presumir que ausência de duplicatas prova completude. Se faltarem páginas ou continuação, marque inconclusivo. Pare diante de erros e limites; não execute coleta em massa.

## Aceite e promoção

Associe cada observação sanitizada à operação, versão e data, identifique claramente fonte histórica versus resultado novo, e abra revisão. `HISTORY-PATH-001`, `HISTORY-SHAPE-001`, `HISTORY-PAGE-001`, formato HTML e necessidade de headers permanecem UNVERIFIED até evidência específica revisada; parte de um contrato pode ser confirmada sem promover o restante. Fixtures e código em commit comprovam construção/parsing local, não requisitos do servidor. Mesmo com evidência, restrinja qualquer VERIFIED ao escopo realmente observado; não feche #5 automaticamente.
