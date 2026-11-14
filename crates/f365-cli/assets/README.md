# Ícone do executável

`fast365.ico` foi gerado a partir do JPEG RGB fornecido pelo dono (2048×2048),
com redimensionamento para 16, 24, 32, 48, 64, 128 e 256 pixels. O xadrez
escuro faz parte dos pixels originais: o ícone é opaco, não transparente.
Nenhuma remoção de fundo foi realizada.

A build do `f365` inclui o recurso apenas para o alvo Windows. Para verificar
os recursos do executável sem executá-lo, no Windows:

```text
cargo build --locked -p f365-cli --features webview
python crates/f365-cli/tests/windows_icon.py target/debug/f365.exe
```
