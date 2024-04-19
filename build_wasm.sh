cargo build \
    --no-default-features \
    --target wasm32-unknown-unknown \
    --lib \
    --profile web-release\
&& wasm-bindgen \
    --out-dir public \
    --target web\
    --no-typescript\
    target/wasm32-unknown-unknown/web-release/v4dv.wasm \
    