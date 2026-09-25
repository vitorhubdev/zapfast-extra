# ZapExt comparado con ZapFast 0.16.2

Comparado con la etiqueta `v0.16.2` de ZapFast, revisada el 2026-09-24.
Esta página lista solo diferencias comprobadas en este árbol o en esa
revisión. No es el registro completo. Los créditos y la licencia MIT siguen
en el [README](../README.md).

[English](differences.md) · [Português](differences.pt-BR.md)

## Validado en este árbol

- El título de la ventana y `--version` muestran `ZapExt` y la versión del
  fork leída de `VERSION`, no la versión de ZapFast.
- Los ajustes tienen un canal de actualización: Estable, o Pruebas, que
  también puede instalar candidatas. Los borradores de GitHub nunca se
  ofrecen.
- No hay selección de varios mensajes con Ctrl-clic o Shift-clic. La
  selección de aquí es de texto, incluso un arrastre que sigue fuera de la
  lista. ZapFast 0.16.2 añadió la selección múltiple de mensajes; ese cambio
  no se aplica a esta selección.
- Las etiquetas de chat, los canales y la columna de salida del archivo de
  esa versión de ZapFast no están en este árbol.
- El reordenamiento de números dentro de líneas de derecha a izquierda de
  esa versión no está en este árbol. Los párrafos en hebreo y árabe solo
  reordenan los tramos de la fuente.

## Implementado, soltar nativo aún no validado

- En Windows, una foto, un vídeo o un documento ya descargado se puede
  arrastrar hacia una carpeta. El shell solo recibe copiar, también con
  Shift. El original permanece. `DoDragDrop` vuelve cuando el `Drop` del
  destino vuelve, o cuando el arrastre se cancela. Eso no dice que el destino
  terminó de leer la ruta ni que no la abrirá otra vez. Un arrastre cancelado
  borra el nombre temporal. Una copia aceptada queda en `drag-export` hasta
  una limpieza posterior. Esa limpieza solo quita archivos ociosos de más de
  un día, y nunca uno que esté abierto. Quien espera más de un día para
  abrirlo, y no lo tiene abierto, ya no encuentra el nombre. Si el enlace
  falla, ZapExt lo explica
  y señala Guardar una copia. En esta máquina, preparar un archivo de 8 MB
  (la carpeta de exportación y el hard link, sin leer los bytes) tardó 1 ms.
  Es solo el tiempo de preparación. No muestra si el arrastre se siente
  fluido. Soltar en el Explorador no se ejecutó en este pase. Usa la lista
  de abajo antes de dar el gesto por confirmado.

## Guía del arrastre en Windows

1. Usa solo archivos sintéticos: una foto, un vídeo y un documento, con un
   nombre acentuado.
2. Arrastra cada uno a una carpeta vacía. Compara el hash de la copia con el
   original. El original tiene que seguir siendo el mismo archivo.
3. Mantén Shift durante un arrastre. El resultado tiene que ser una copia,
   no un movimiento.
4. Pulsa Escape y suelta en un sitio que rechace el archivo. El original
   tiene que seguir ahí.
5. No borres `drag-export`. Cuando la copia esté en la carpeta de destino,
   comprueba que se abre. La aplicación quita exportaciones ociosas de más
   de un día.
6. Arrastra un documento cuyo nombre ya existe en la carpeta de espera, y un
   archivo en un volumen que no acepte un hard link. ZapExt debe explicar el
   fallo y dejar Guardar una copia en el menú.
7. Durante el arrastre, el mensaje no se abre, la selección de texto del
   cuerpo sigue, y la barra del vídeo sigue buscando.
