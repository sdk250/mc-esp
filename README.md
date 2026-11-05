# mc-esp

Minecraft 基岩版（Android）的外部透视，用于单人世界下学习逆向和图形编程。需要 root。

内存偏移只对特定游戏版本有效，游戏更新后要重新定位。

## 构建和运行

渲染层是独立项目 [`android-overlay`](../android-overlay)，要和本项目放在同一级目录下。屏幕分辨率在启动时自动获取。

```sh
cargo ndk -t arm64-v8a -P 30 build --release
```

把 `target/aarch64-linux-android/release/mc-esp` 传到手机上，以 root 运行：

```sh
./mc-esp
```

## 环境变量

变量都在运行时设置，比如 `MC_ESP_BOX=2d MC_ESP_SKELETON=1 ./mc-esp`。每次进入世界时读取一次，运行中途无法修改，要改就重新启动程序。

| 变量 | 取值 | 默认 | 作用 |
|---|---|---|---|
| `MC_ESP_BOX` | `0` / `off` / `false`：不画方框<br>`2d`：屏幕矩形<br>其他值：3D 方框 | 3D 方框 | 方框样式，大小写不限。名字和距离标签不受影响 |
| `MC_ESP_SKELETON` | `1` / `on` / `true`：画骨架<br>其他值：不画 | 不画 | 是否画骨架，大小写不限。视线不受影响，一直显示 |
| `MC_ESP_VIEW_DELAY_MS` | 非负整数（毫秒） | `30` | 视角延后的时间，用来对齐游戏画面。值无效时使用默认值 |
| `MC_ESP_PROBE` | 设置为任意值 | 不设置 | 逆向用：进入世界时打印候选组件存储池的原始数据 |

注意 `MC_ESP_PROBE` 只看变量是否存在，`MC_ESP_PROBE=0` 同样会开启。

启动日志里的 `box:` 和 `skeleton:` 两行显示这两个开关当前的状态。
