# Auditoria Windows — crash ao restaurar pela barra/tray

## Sintoma
Ao acessar/restaurar o app pela barra lateral/tray/área próxima ao Menu Iniciar, a tela/app pode crashar.

## Arquitetura atual relevante
- tray nativo roda em thread própria;
- evento gera `TrayCommand::ShowHide`;
- App transforma em `Action::ShowWindow` ou `HideWindow`;
- ao esconder, o viewport é fechado e o objeto App é preservado;
- ao mostrar novamente, `eframe::run_native` é chamado outra vez para recriar a janela;
- há também `winfocus::raise()`, que usa EnumWindows, ShowWindow(SW_RESTORE), BringWindowToTop, AttachThreadInput e SetForegroundWindow;
- essa tentativa é repetida a cada 250 ms por até 1.5 s.

## Risco principal encontrado
A estratégia combina duas operações potencialmente conflitantes:
1. recriação completa do viewport/GL context via novo `run_native`;
2. rotina Win32 buscando/restaurando/focando uma HWND durante a janela de criação.

Durante o intervalo em que a nova janela ainda não está totalmente criada/estável, `winfocus::raise()` pode encontrar uma HWND antiga, intermediária ou recém-criada e operar sobre ela. Isso é uma forte candidata a race/instabilidade, especialmente com `panic=abort`, onde qualquer panic encerra o processo sem unwinding.

## Outros fatores
- `GlowConfiguration { vsync: false }` é aplicado a todas as plataformas apesar do comentário tratar de Wayland;
- `panic = "abort"` transforma panics recuperáveis em crash imediato;
- vários `.expect("application state present")` assumem invariantes do ciclo de recriação;
- o foco é repetido mesmo após a janela possivelmente já estar ativa.

## Correção recomendada

### P0
Separar "mostrar" de "forçar foreground":
- primeiro recriar janela;
- somente depois do primeiro frame/handle válido, disparar raise;
- cancelar retries assim que viewport relata focused=true;
- nunca chamar raise durante headless/context inexistente.

### P0
No Windows, testar manter uma janela viva e apenas `Visible(false/true)` em vez de fechar/recriar. Isso reduz drasticamente a superfície de race de GL/Winit/Win32 e preserva resources.

### P1
Trocar busca por título por handle explícito, quando possível. Buscar HWND por prefixo "ZapExt" é frágil.

### P1
Restringir workaround de vsync ao Linux/Wayland. Medir Windows com vsync padrão.

### P1
Adicionar telemetria local:
- log antes/depois de ShowHide;
- hidden/wants_show/hide_intent;
- viewport focused/minimized;
- HWND encontrada;
- retorno das APIs Win32;
- thread id;
- panic log com backtrace em builds de diagnóstico.

## Reproduções a testar
1. minimizar pela taskbar e restaurar;
2. esconder pelo tray e clicar uma vez;
3. duplo clique rápido;
4. spam 10x no ícone;
5. notificação enquanto oculto;
6. Alt+Tab durante restore;
7. monitor secundário/DPI diferente;
8. sleep/resume;
9. fullscreen app em foreground.

## Hipótese de maior probabilidade
Race entre recriação do viewport/context e `winfocus::raise()`, amplificada pelos retries e pelo modelo de fechar/reabrir janela.

## Observação
Sem dump/stack trace do crash não é possível provar a linha exata. Esta auditoria identifica o caminho de maior risco no código atual e define instrumentação para fechar o diagnóstico.
