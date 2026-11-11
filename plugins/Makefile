include toolchain.env

CORE_DIR := ../core
WIT_DIR := $(CORE_DIR)/wit
OUT := out
BUILD := build
TOOLCHAIN := $(CURDIR)/.toolchain
STAGES := parse filter format transport

.PHONY: all clean rust check-rust install-rust verify-rust setup-c c check-c verify-c \
	setup-csharp csharp check-csharp verify-csharp setup-go go check-go verify-go

all: rust c csharp go

# ── Rust ─────────────────────────────────────────────────────────────────────

RUST_RELEASE := rust/target/$(RUST_TARGET)/release

# 編譯 Rust 插件到 out/rust 與 out/fault
rust: check-rust
	cd rust && cargo build --release --locked --target $(RUST_TARGET)
	mkdir -p $(OUT)/rust $(OUT)/fault
	for s in $(STAGES); do \
		cp $(RUST_RELEASE)/$$s.wasm $(OUT)/rust/$$s.wasm; \
		cp $(RUST_RELEASE)/fault_$$s.wasm $(OUT)/fault/$$s.wasm; \
	done
	@echo "built: $(OUT)/rust $(OUT)/fault"

check-rust:
	@rustc --version | grep -q "rustc $(RUST_VERSION) " || { echo "需要 rustc $(RUST_VERSION)，目前是 $$(rustc --version)"; exit 1; }
	@rustup target list --installed | grep -qx "$(RUST_TARGET)" || { echo "缺少 target，請執行：rustup target add $(RUST_TARGET)"; exit 1; }

# 把編譯好的 Rust 插件更新到 core/plugin
install-rust: rust
	mkdir -p $(CORE_DIR)/plugin/fault
	for s in $(STAGES); do \
		cp $(OUT)/rust/$$s.wasm $(CORE_DIR)/plugin/$$s.wasm; \
		cp $(OUT)/fault/$$s.wasm $(CORE_DIR)/plugin/fault/$$s.wasm; \
	done

# 用 core 的 forwarder 實際跑過（需要先在 core 執行 cargo build --release）
verify-rust: rust
	python3 verify/verify.py rust

# ── C ────────────────────────────────────────────────────────────────────────

WASI_SDK := $(TOOLCHAIN)/wasi-sdk-$(WASI_SDK_VERSION)-x86_64-linux
CLANG := $(WASI_SDK)/bin/clang
WIT_BINDGEN := $(TOOLCHAIN)/bin/wit-bindgen
CJSON_DIR := $(TOOLCHAIN)/cjson-$(CJSON_VERSION)
# wasm32-wasip2 由 wasm-component-ld 直接產生 component；reactor 代表沒有 main，只有匯出函式
C_FLAGS := --target=wasm32-wasip2 -mexec-model=reactor -O2 -Wall

# 每個階段對應的 WIT world 與額外的原始碼
C_WORLD_parse := parser-plugin
C_WORLD_filter := reduction-plugin
C_WORLD_format := format-plugin
C_WORLD_transport := transport-plugin
C_EXTRA_parse := -I$(CJSON_DIR) $(CJSON_DIR)/cJSON.c

# 下載並安裝 C 工具鏈到 .toolchain/
setup-c:
	scripts/setup-c.sh

check-c:
	@test -x $(CLANG) -a -x $(WIT_BINDGEN) -a -f $(CJSON_DIR)/cJSON.c || { echo "缺少 C 工具鏈，請先執行：make setup-c"; exit 1; }

c: $(STAGES:%=$(OUT)/c/%.wasm)
	@echo "built: $(OUT)/c"

# 1. 從 WIT 產生 C 綁定（<world>.h／.c，以及記錄型別資訊的 _component_type.o）
# 2. 與手寫的原始碼一起編譯成 component
$(OUT)/c/%.wasm: c/%/*.c $(WIT_DIR)/log_plugin.wit | check-c
	mkdir -p $(BUILD)/c/$* $(OUT)/c
	$(WIT_BINDGEN) c --world $(C_WORLD_$*) --out-dir $(BUILD)/c/$* $(WIT_DIR)
	$(CLANG) $(C_FLAGS) -I$(BUILD)/c/$* $(C_EXTRA_$*) c/$*/*.c $(BUILD)/c/$*/*.c $(BUILD)/c/$*/*_component_type.o -o $@

verify-c: c
	python3 verify/verify.py c

# ── C# ───────────────────────────────────────────────────────────────────────

# 下載並安裝 .NET SDK、wasi-sdk 與 NuGet 套件到 .toolchain/
setup-csharp:
	scripts/setup-csharp.sh

check-csharp:
	@test -x $(TOOLCHAIN)/dotnet/dotnet -a -x $(CLANG) -a -d $(TOOLCHAIN)/nuget || { echo "缺少 C# 工具鏈，請先執行：make setup-csharp"; exit 1; }

csharp: $(STAGES:%=$(OUT)/csharp/%.wasm)
	@echo "built: $(OUT)/csharp"

# componentize-dotnet 在建置時產生綁定，NativeAOT-LLVM 編譯並輸出 component。
# WasiSdkRoot 指向 .toolchain 內的 wasi-sdk，否則它會自行下載到 ~/.wasi-sdk。
$(OUT)/csharp/%.wasm: csharp/%/*.cs csharp/%/*.csproj csharp/%/packages.lock.json csharp/Directory.Build.props $(WIT_DIR)/log_plugin.wit | check-csharp
	. scripts/dotnet-env.sh && dotnet build csharp/$* -c Release -p:RestoreLockedMode=true -p:WasiSdkRoot=$(WASI_SDK)
	mkdir -p $(OUT)/csharp
	cp $(BUILD)/csharp/$*/bin/Release/net10.0/wasi-wasm/native/$*.wasm $@

verify-csharp: csharp
	python3 verify/verify.py csharp

# ── Go ───────────────────────────────────────────────────────────────────────

GO_WIT := $(BUILD)/go/wit
GO_BINDINGS := go/internal

# 每個階段對應的 world（go/go-worlds.wit：core 的 world 加上 TinyGo 需要的 wasi:cli import）
GO_WORLD_parse := parser-plugin-go
GO_WORLD_filter := reduction-plugin-go
GO_WORLD_format := format-plugin-go
GO_WORLD_transport := transport-plugin-go

# 下載並安裝 Go、TinyGo、wasm-tools、wit-bindgen-go 與 Go 模組到 .toolchain/
setup-go:
	scripts/setup-go.sh

check-go:
	@test -x $(TOOLCHAIN)/go/bin/go -a -x $(TOOLCHAIN)/tinygo/bin/tinygo -a -x $(TOOLCHAIN)/bin/wasm-tools -a -x $(TOOLCHAIN)/bin/wit-bindgen-go || { echo "缺少 Go 工具鏈，請先執行：make setup-go"; exit 1; }

go: $(STAGES:%=$(OUT)/go/%.wasm)
	@echo "built: $(OUT)/go"

# 1. 組合 WIT：core 的 WIT 加上 go-worlds.wit
# 2. 從 WIT 產生 Go 綁定到 go/internal（四個 world 共用的型別會產生到同一個位置）
$(GO_BINDINGS)/.stamp: $(WIT_DIR)/log_plugin.wit go/go-worlds.wit | check-go
	mkdir -p $(BUILD)/go
	cp -r $(WIT_DIR)/. $(GO_WIT)
	cp go/go-worlds.wit $(GO_WIT)/
	for w in $(GO_WORLD_parse) $(GO_WORLD_filter) $(GO_WORLD_format) $(GO_WORLD_transport); do \
		. scripts/go-env.sh && wit-bindgen-go generate --world $$w --out $(GO_BINDINGS) $(GO_WIT) || exit 1; \
	done
	touch $@

# 3. TinyGo 編譯成 component（以 --wit-package／--wit-world 嵌入介面，再呼叫 wasm-tools 包裝）
#    -scheduler=none：插件不使用 goroutine，省去 asyncify 轉換（吞吐量約為預設的 1.6 倍）
#    -opt=2：以速度優先（預設 -opt=z 以體積優先）
TINYGO_FLAGS := -target=wasip2 -scheduler=none -opt=2
$(OUT)/go/%.wasm: go/%/*.go go/go.mod go/go.sum $(GO_BINDINGS)/.stamp | check-go
	mkdir -p $(OUT)/go
	. scripts/go-env.sh && cd go && tinygo build $(TINYGO_FLAGS) --wit-package ../$(GO_WIT) --wit-world $(GO_WORLD_$*) -o ../$@ ./$*

verify-go: go
	python3 verify/verify.py go

# ─────────────────────────────────────────────────────────────────────────────

clean:
	rm -rf $(OUT) $(BUILD) $(GO_BINDINGS)
	cd rust && cargo clean
