# Ícone do executável

`fast365.ico` foi gerado com Pillow 13.0.0 a partir do JPEG RGB fornecido pelo dono
`dashboard_20261114_163814_dc11734a_F365_icon_final_a3f7d4.jpg` (2048×2048;
SHA-256 `5a27ce1084ee22dc2851ffb7aa86eb2b198f7bff8e7cb08e9986a7eb44274974`),
com redimensionamento para 16, 24, 32, 48, 64, 128 e 256 pixels. O xadrez
escuro faz parte dos pixels originais: o ícone é opaco, não transparente.
Nenhuma remoção de fundo foi realizada.

A build do `f365` inclui o recurso apenas para o alvo Windows. Para verificar
os recursos do executável sem executá-lo, no Windows:

```text
cargo build --locked -p f365-cli --features webview
python crates/f365-cli/tests/windows_icon.py target/debug/f365.exe
```
