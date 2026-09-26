# Vespera comparado com o ZapFast 0.16.2

Comparado com a tag `v0.16.2` do ZapFast, revisada em 2026-09-24.
Esta página lista só diferenças conferidas nesta árvore ou nessa revisão.
Não é o changelog completo. Os créditos e a licença MIT continuam no
[README](../README.md).

[English](differences.md) · [Español](differences.es.md)

## Validado nesta árvore

- O título da janela e o `--version` mostram `Vespera` e a versão do fork lida
  de `VERSION`, não a versão do Vespera.
- As configurações têm um canal de atualização: Estável, ou Teste, que também
  pode instalar candidatos. Rascunhos do GitHub nunca são oferecidos.
- Não há seleção de várias mensagens com Ctrl-clique ou Shift-clique.
  A seleção daqui é de texto, inclusive um arraste que continua fora da lista.
  O ZapFast 0.16.2 adicionou a seleção múltipla de mensagens; esse commit não
  se aplica a esta seleção.
- Etiquetas de conversa, canais e a coluna de saída do arquivo dessa versão
  do Vespera não estão nesta árvore.
- A reordenação de números dentro de linhas da direita para a esquerda dessa
  versão não está nesta árvore. Parágrafos em hebraico e árabe só reordenam
  os trechos da fonte.

## Implementado, drop nativo ainda não validado

- No Windows, uma foto, um vídeo ou um documento já baixado pode ser
  arrastado em direção a uma pasta. O shell recebe só o efeito copiar,
  também com Shift. O original permanece. `DoDragDrop` volta quando o `Drop`
  do destino volta, ou quando o arraste é cancelado. Isso não diz que o
  destino terminou de ler o caminho nem que não vai abri-lo de novo. Um
  arraste cancelado apaga o nome temporário. Uma cópia aceita fica em
  `drag-export` até uma limpeza posterior. Essa limpeza só tira arquivos
  ociosos com mais de um dia, e nunca um que esteja aberto. Quem espera mais
  de um dia para abrir, e não está com o arquivo aberto, não encontra mais
  o nome. Se o link falha,
  o Vespera explica e aponta Salvar como. Nesta máquina, preparar um arquivo
  de 8 MB (a pasta de exportação e o hard link, sem ler os bytes) levou 1 ms.
  Isso é só o tempo da preparação. Não mostra se o arraste parece fluido.
  Soltar no Explorer não foi executado nesta passagem.
  Use a lista abaixo antes de tratar o gesto como confirmado.

## Roteiro do arraste no Windows

1. Use só arquivos sintéticos: uma foto, um vídeo e um documento, com um
   nome acentuado.
2. Arraste cada um para uma pasta vazia. Compare o hash da cópia com o
   original. O original tem de continuar o mesmo arquivo.
3. Segure Shift durante um arraste. O resultado tem de ser uma cópia, não
   uma mudança de lugar.
4. Aperte Escape e solte num lugar que recuse o arquivo. O original tem de
   continuar.
5. Não apague `drag-export`. Depois que a cópia estiver na pasta de destino,
   confira se ela abre. O aplicativo remove exportações ociosas com mais de
   um dia.
6. Arraste um documento cujo nome já existe na pasta de espera, e um arquivo
   num volume que não aceite hard link. O Vespera deve explicar a falha e
   manter Salvar como no menu.
7. Durante o arraste, a mensagem não abre, a seleção de texto do corpo
   continua, e a barra do vídeo continua buscando.
