// overlay.cpp：在 SurfaceFlinger 的最顶层创建一个透明、触摸穿透的图层，返回它的 ANativeWindow*
//
// 依赖的是 Android 17（SDK 37）libgui 的私有接口。NDK 里没有这些头文件，所以：
//   - 函数通过 dlsym 按 mangled 名字查找。AArch64 上，C++ 成员函数就是“第一个参数是 this”的普通函数；
//   - 对象的真实大小未知，一律用一块“足够大”的清零内存代替，再调用它自己的构造函数；
//   - 和 libgui 之间只传指针和整数，不传任何 std 类型（libgui 用的是系统 libc++，我们用的是 NDK 的）。
//
// 换系统版本后，最可能失效的是 createSurface / apply 的签名，用 llvm-nm -D libgui.so 重新核对。
#include <android/native_window.h>
#include <dlfcn.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>

namespace {

// android::sp<T> 在内存里只有一个指针。
//
// 写出非平凡的拷贝构造和析构，是为了让编译器把它当成“不能放进寄存器返回”的类型：
// 按 Itanium C++ ABI，这种类型的返回值由调用方分配内存，地址通过 x8 传入——和真正的 sp<T> 一致。
// 如果写成平凡的 struct，编译器会期望返回值在 x0 里，就和 libgui 对不上了。
//
// 析构故意什么都不做：图层要一直活到进程结束，不需要减引用计数。
struct Sp {
    void* ptr = nullptr;
    Sp() = default;
    Sp(const Sp& o) : ptr(o.ptr) {}
    ~Sp() {}
};

// 为大小未知的 C++ 对象分配内存。宁大勿小：分配多了只是浪费，分配少了会踩坏堆
void* blob(size_t n) { return calloc(1, n); }

void* sym(void* lib, const char* name) {
    void* p = dlsym(lib, name);
    if (!p) fprintf(stderr, "[overlay] 缺少符号 %s\n", name);
    return p;
}

// SurfaceFlinger 的像素格式枚举：PIXEL_FORMAT_RGBA_8888
constexpr int32_t kPixelFormatRgba8888 = 1;

// ANativeWindow 结构体开头的魔数 '_wnd'（ANDROID_NATIVE_WINDOW_MAGIC）
constexpr uint32_t kWindowMagic = ('_' << 24) | ('w' << 16) | ('n' << 8) | 'd';

}  // namespace

// 创建图层，返回可以交给 vkCreateAndroidSurfaceKHR 的 ANativeWindow*；失败返回 nullptr，原因打印到 stderr。
//
// w、h 是图层的尺寸，单位是“逻辑显示坐标”的像素：这个坐标系跟着屏幕旋转，横屏时宽 > 高。
extern "C" ANativeWindow* overlay_create(uint32_t w, uint32_t h) {
    void* gui = dlopen("libgui.so", RTLD_NOW);
    void* utils = dlopen("libutils.so", RTLD_NOW);
    if (!gui || !utils) {
        fprintf(stderr, "[overlay] dlopen 失败: %s\n", dlerror());
        return nullptr;
    }

    // ---- 函数签名（对应 SDK 37 的声明） ----
    using Ctor = void (*)(void* self);
    using String8Ctor = void (*)(void* self, const char* s);
    using IncStrong = void (*)(const void* self, const void* id);
    // sp<SurfaceControl> SurfaceComposerClient::createSurface(const String8& name, uint32_t w, uint32_t h,
    //     PixelFormat format, int32_t flags, const sp<IBinder>& parent, const LayerMetadata& metadata,
    //     uint32_t* outTransformHint)
    using CreateSurface = Sp (*)(void* self, const void* name, uint32_t w, uint32_t h,
                                 int32_t format, int32_t flags, const Sp* parent,
                                 const void* metadata, uint32_t* outTransformHint);
    // Transaction 的 setXxx 都返回 Transaction&，我们不用返回值
    using SetLayer = void* (*)(void* self, const Sp* sc, int32_t z);
    using Show = void* (*)(void* self, const Sp* sc);
    using SetTrusted = void* (*)(void* self, const Sp* sc, bool trusted);
    using Apply = int32_t (*)(void* self, bool synchronous, bool oneWay);
    using GetSurface = Sp (*)(void* self);

    auto client_ctor = (Ctor)sym(gui, "_ZN7android21SurfaceComposerClientC1Ev");
    auto create_surface = (CreateSurface)sym(gui,
        "_ZN7android21SurfaceComposerClient13createSurfaceERKNS_7String8EjjiiRKNS_2spINS_7IBinderEEERKNS_3gui13LayerMetadataEPj");
    auto meta_ctor = (Ctor)sym(gui, "_ZN7android3gui13LayerMetadataC1Ev");
    auto tx_ctor = (Ctor)sym(gui, "_ZN7android21SurfaceComposerClient11TransactionC1Ev");
    auto set_layer = (SetLayer)sym(gui,
        "_ZN7android21SurfaceComposerClient11Transaction8setLayerERKNS_2spINS_14SurfaceControlEEEi");
    auto show = (Show)sym(gui,
        "_ZN7android21SurfaceComposerClient11Transaction4showERKNS_2spINS_14SurfaceControlEEE");
    auto set_trusted = (SetTrusted)sym(gui,
        "_ZN7android21SurfaceComposerClient11Transaction17setTrustedOverlayERKNS_2spINS_14SurfaceControlEEEb");
    auto apply = (Apply)sym(gui, "_ZN7android21SurfaceComposerClient11Transaction5applyEbb");
    auto get_surface = (GetSurface)sym(gui, "_ZN7android14SurfaceControl10getSurfaceEv");
    auto string8_ctor = (String8Ctor)sym(utils, "_ZN7android7String8C1EPKc");
    auto inc_strong = (IncStrong)sym(utils, "_ZNK7android7RefBase9incStrongEPKv");
    if (!client_ctor || !create_surface || !meta_ctor || !tx_ctor || !set_layer || !show ||
        !set_trusted || !apply || !get_surface || !string8_ctor || !inc_strong) {
        return nullptr;
    }

    // ---- 1. 连接 SurfaceFlinger ----
    // 构造函数只初始化成员；真正的连接在 onFirstRef() 里建立，
    // 而 onFirstRef() 由 RefBase 在“第一次 incStrong”时调用，所以这里手动 incStrong 一次
    void* client = blob(0x1000);
    client_ctor(client);
    inc_strong(client, client);

    // ---- 2. 创建图层 ----
    // flags = 0：不加 eOpaque，SurfaceFlinger 才会按像素的 alpha 混合（Vulkan 侧对应 compositeAlpha = INHERIT）
    // parent 为空：作为根图层。只有拥有 SurfaceFlinger 特权的进程（root 满足）才能这样做
    void* name = blob(0x40);
    string8_ctor(name, "mc-esp-overlay");
    void* metadata = blob(0x400);
    meta_ctor(metadata);
    Sp no_parent;
    Sp sc = create_surface(client, name, w, h, kPixelFormatRgba8888, 0, &no_parent, metadata, nullptr);
    if (!sc.ptr) {
        fprintf(stderr, "[overlay] createSurface 返回空\n");
        return nullptr;
    }

    // ---- 3. 用一个事务设置图层属性 ----
    void* tx = blob(0x4000);
    tx_ctor(tx);
    set_layer(tx, &sc, 0x7FFFFFFF);  // z 序最大：盖在所有窗口上面
    show(tx, &sc);
    // 标记为可信覆盖层。Android 12+ 会丢弃“穿过不可信且不透明度 > 0.8 的覆盖层”的触摸（防点击劫持）。
    // 我们的图层属于 uid 0，对游戏来说是别人的覆盖层，不标记的话游戏收不到任何触摸
    set_trusted(tx, &sc, true);
    int32_t err = apply(tx, false, false);
    if (err != 0) fprintf(stderr, "[overlay] Transaction::apply 返回 %d\n", err);

    // ---- 4. 取出 Surface，在其中定位 ANativeWindow ----
    Sp surface = get_surface(sc.ptr);
    if (!surface.ptr) {
        fprintf(stderr, "[overlay] getSurface 返回空\n");
        return nullptr;
    }
    // ANativeWindow 是 Surface 的一个基类，但它在对象里的偏移随版本变化（SDK 37 上是 +0x10，
    // 前面多了别的基类），所以按结构体开头的魔数找，而不是假设偏移为 0
    auto* bytes = static_cast<uint8_t*>(surface.ptr);
    for (size_t off = 0; off < 0x100; off += 8) {
        if (*reinterpret_cast<uint32_t*>(bytes + off) == kWindowMagic) {
            return reinterpret_cast<ANativeWindow*>(bytes + off);
        }
    }
    fprintf(stderr, "[overlay] 在 Surface 里没找到 ANativeWindow 魔数\n");
    return nullptr;
}
