//! Vulkan 渲染器：把一组矩形边框画到覆盖图层上
//!
//! 不用 shader 和图形管线：每个框拆成上、下、左、右四条实心细长矩形，
//! 在 RenderPass 里用 vkCmdClearAttachments 把这些区域直接“清”成框的颜色。
//!
//! 坐标系是图层的坐标系：逻辑显示坐标（横屏时就是游戏画面的坐标），原点在左上角，x 向右、y 向下，单位像素。
//!
//! 有必要说明一下，这部分代码是由 Anthropic 公司的 AI 模型 Claude5.5编写的，我并不具备 Rust 图形开发的经验
//! 更别提这是完全基于底层 Vulkan API 进行渲染，其中有非常多的图形接口的学习成本都是非常高的

use std::ffi::c_void;
use std::ptr::NonNull;

use ash::vk;
use tracing::info;

use crate::error::{ ESPError, Result };


const MODULE: &str = "Renderer";

/// 交换链图像格式。用 UNORM 而不是 SRGB：写进去的值就是存下的值，不做 gamma 变换
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;


/// 一个矩形边框
#[derive(Debug, Clone, Copy)]
pub struct Outline {
    /// 左、上、右、下（像素，图层坐标系）
    pub rect: [f32; 4],
    /// 非预乘的 RGBA，各分量 0.0..=1.0。预乘由渲染器完成
    pub color: [f32; 4],
    /// 线宽（像素）
    pub thickness: f32,
    /// 框内的填充色（非预乘 RGBA），None 表示不填充。想要半透明就把 alpha 设小一点
    pub fill: Option<[f32; 4]>,
}


/// begin_frame 拿到的交换链图像，交给 end_frame 使用
pub struct Frame {
    image_index: u32,
}


/// 把 vk::Result 转成 ESPError 错误类型，`what` 写明是哪一步失败
fn vk_err(what: &'static str) -> impl Fn(vk::Result) -> ESPError {
    move |r| ESPError::Custom(MODULE, format!("{what}: {r:?}").into())
}


/// 持有所有 Vulkan 对象。创建一次，每帧调用 `draw`
///
/// 字段顺序就是创建顺序，`Drop` 里按相反顺序销毁。
pub struct Renderer {
    _entry: ash::Entry,   // 持有 libvulkan.so，必须比所有 Vulkan 对象活得久
    instance: ash::Instance,
    surface_fn: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    device: ash::Device,
    queue: vk::Queue,
    swapchain_fn: ash::khr::swapchain::Device,
    swapchain: vk::SwapchainKHR,
    views: Vec<vk::ImageView>,
    render_pass: vk::RenderPass,
    framebuffers: Vec<vk::Framebuffer>,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    /// acquire 拿到图像时由驱动触发
    image_ready: vk::Semaphore,
    /// 某张图像画完、可以显示时触发。每张图像一个，避免上一次 present 还没用完就被复用
    render_done: Vec<vk::Semaphore>,
    /// GPU 执行完本帧命令时触发，CPU 等它之后才能重录命令缓冲
    in_flight: vk::Fence,
    /// 交换链图像尺寸，等于图层尺寸
    pub extent: vk::Extent2D,
    /// 实际使用的显示模式。MAILBOX 时 begin_frame 不会等垂直同步，调用方要自己控制帧率
    pub present_mode: vk::PresentModeKHR,
}

impl Renderer {
    /// 在 `window`（overlay::create_window 的返回值）上初始化 Vulkan，`width`、`height` 是图层尺寸
    ///
    /// 中途失败时已创建的对象不会被释放；这只发生在启动阶段，进程随后就退出了。
    pub fn new(window: NonNull<c_void>, width: u32, height: u32) -> Result<Self> {
        unsafe { Self::init(window, width, height) }
    }

    unsafe fn init(window: NonNull<c_void>, width: u32, height: u32) -> Result<Self> { unsafe {
        // ---- Instance：加载驱动，启用“从 ANativeWindow 创建 surface”所需的两个扩展 ----
        let entry = ash::Entry::load()
            .map_err(|e| ESPError::Custom(MODULE, format!("load libvulkan.so: {e}").into()))?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let instance_exts = [
            ash::khr::surface::NAME.as_ptr(),
            ash::khr::android_surface::NAME.as_ptr(),
        ];
        let instance = entry.create_instance(
            &vk::InstanceCreateInfo::default()
                .application_info(&app)
                .enabled_extension_names(&instance_exts),
            None,
        ).map_err(vk_err("create_instance"))?;

        // ---- Surface：Vulkan 对图层的抽象 ----
        let android_surface = ash::khr::android_surface::Instance::new(&entry, &instance);
        let surface = android_surface.create_android_surface(
            &vk::AndroidSurfaceCreateInfoKHR::default().window(window.as_ptr().cast()),
            None,
        ).map_err(vk_err("create_android_surface"))?;
        let surface_fn = ash::khr::surface::Instance::new(&entry, &instance);

        // ---- 物理设备 + 队列族：找一个能画图、又能 present 到这个 surface 的队列族 ----
        let mut chosen = None;
        for pd in instance.enumerate_physical_devices().map_err(vk_err("enumerate_physical_devices"))? {
            let families = instance.get_physical_device_queue_family_properties(pd);
            for (i, f) in families.iter().enumerate() {
                let present = surface_fn
                    .get_physical_device_surface_support(pd, i as u32, surface)
                    .unwrap_or(false);
                if present && f.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                    chosen = Some((pd, i as u32));
                    break;
                }
            }
            if chosen.is_some() { break; }
        }
        let (pd, qf) = chosen.ok_or(ESPError::Custom(MODULE, "no graphics+present queue family".into()))?;

        // ---- 逻辑设备和队列。交换链是设备级扩展 ----
        let prio = [1.0f32];
        let queue_ci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qf)
            .queue_priorities(&prio)];
        let device_exts = [ash::khr::swapchain::NAME.as_ptr()];
        let device = instance.create_device(
            pd,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_ci)
                .enabled_extension_names(&device_exts),
            None,
        ).map_err(vk_err("create_device"))?;
        let queue = device.get_device_queue(qf, 0);

        // ---- 交换链 ----
        let caps = surface_fn
            .get_physical_device_surface_capabilities(pd, surface)
            .map_err(vk_err("get_physical_device_surface_capabilities"))?;
        // 交换链图像尺寸用图层尺寸，而不是 current_extent：在 Android 上，当系统给出 90°/270° 的
        // 变换提示（current_transform）时，current_extent 可能是宽高互换后的值
        let extent = vk::Extent2D {
            width: width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
            height: height.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
        };
        info!(
            "surface: current_extent={}x{}, current_transform={:?}, using {}x{} + IDENTITY",
            caps.current_extent.width, caps.current_extent.height, caps.current_transform,
            extent.width, extent.height
        );
        // 背景要透明：优先 INHERIT（Android 上由 SurfaceFlinger 按预乘 alpha 混合），其次 PRE_MULTIPLIED
        let composite_alpha = [
            vk::CompositeAlphaFlagsKHR::INHERIT,
            vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED
        ]
            .into_iter()
            .find(|&a| caps.supported_composite_alpha.contains(a))
            .ok_or(ESPError::Custom(MODULE, "surface does not support transparent composite alpha".into()))?;

        // 显示模式：优先 MAILBOX——总是显示最新画好的一帧，旧帧直接丢弃，不排队。
        // FIFO 下所有图像要排队依次显示，这台机器的交换链至少 5 张图像，最坏要等 4 帧才轮到最新的画面，
        // 转动视角时框就会明显拖在后面。没有 MAILBOX 时退回 FIFO（所有驱动都必须支持）
        let present_mode = surface_fn
            .get_physical_device_surface_present_modes(pd, surface)
            .map_err(vk_err("get_physical_device_surface_present_modes"))?
            .into_iter()
            .find(|&m| m == vk::PresentModeKHR::MAILBOX)
            .unwrap_or(vk::PresentModeKHR::FIFO);
        info!("present mode: {:?}", present_mode);

        let swapchain_fn = ash::khr::swapchain::Device::new(&instance, &device);
        let swapchain = swapchain_fn.create_swapchain(
            &vk::SwapchainCreateInfoKHR::default()
                .surface(surface)
                .min_image_count(caps.min_image_count)
                .image_format(FORMAT)
                .image_color_space(vk::ColorSpaceKHR::SRGB_NONLINEAR)
                .image_extent(extent)
                .image_array_layers(1)
                .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT) // 在 RenderPass 里当颜色附件
                .image_sharing_mode(vk::SharingMode::EXCLUSIVE)     // 只有一个队列族在用
                // IDENTITY：我们按图层坐标画，旋转（如果需要）交给合成器做。
                // 如果改用 current_transform，就得自己把画面预先旋转好
                .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
                .composite_alpha(composite_alpha)
                .present_mode(present_mode)
                .clipped(true),
            None,
        ).map_err(vk_err("create_swapchain"))?;
        let images = swapchain_fn.get_swapchain_images(swapchain).map_err(vk_err("get_swapchain_images"))?;

        // ---- 每张交换链图像一个 ImageView ----
        let mut views = Vec::with_capacity(images.len());
        for &image in &images {
            let view = device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(FORMAT)
                    .subresource_range(vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1)),
                None,
            ).map_err(vk_err("create_image_view"))?;
            views.push(view);
        }

        // ---- RenderPass：开始时清成透明，结束时转成可显示的布局 ----
        let attachment = [vk::AttachmentDescription::default()
            .format(FORMAT)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)        // 每帧都整屏清掉，不关心旧内容
            .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
        let color_ref = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpass = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_ref)];
        // acquire 的信号量在 COLOR_ATTACHMENT_OUTPUT 阶段等待；这个依赖保证布局转换也排在它之后
        let dependency = [vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
        let render_pass = device.create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(&attachment)
                .subpasses(&subpass)
                .dependencies(&dependency),
            None,
        ).map_err(vk_err("create_render_pass"))?;

        // ---- 每个 ImageView 一个 Framebuffer ----
        let mut framebuffers = Vec::with_capacity(views.len());
        for &view in &views {
            let attachments = [view];
            let fb = device.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(render_pass)
                    .attachments(&attachments)
                    .width(extent.width)
                    .height(extent.height)
                    .layers(1),
                None,
            ).map_err(vk_err("create_framebuffer"))?;
            framebuffers.push(fb);
        }

        // ---- 命令缓冲：只用一个，每帧重录 ----
        let pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(qf)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        ).map_err(vk_err("create_command_pool"))?;
        let cmd = device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        ).map_err(vk_err("allocate_command_buffers"))?[0];

        // ---- 同步对象 ----
        let sem_ci = vk::SemaphoreCreateInfo::default();
        let image_ready = device.create_semaphore(&sem_ci, None).map_err(vk_err("create_semaphore"))?;
        let mut render_done = Vec::with_capacity(images.len());
        for _ in &images {
            render_done.push(device.create_semaphore(&sem_ci, None).map_err(vk_err("create_semaphore"))?);
        }
        // 初始即为已触发，第一帧的 wait 才不会永远等下去
        let in_flight = device.create_fence(
            &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
            None,
        ).map_err(vk_err("create_fence"))?;

        Ok(Self {
            _entry: entry, instance, surface_fn, surface, device, queue, swapchain_fn, swapchain,
            views, render_pass, framebuffers, pool, cmd, image_ready, render_done, in_flight, extent,
            present_mode,
        })
    }}

    /// 画一帧 = begin_frame + end_frame
    #[allow(dead_code)]
    pub fn draw(&mut self, outlines: &[Outline]) -> Result<()> {
        let frame = self.begin_frame()?;
        self.end_frame(frame, outlines)
    }

    /// 开始一帧：等上一帧的 GPU 工作结束，再向交换链要一张空闲图像
    ///
    /// FIFO 模式下这里可能阻塞到下一次垂直同步。所以要在它**之后**再读游戏内存，
    /// 提交给 GPU 的才是最新的数据。返回值交给 end_frame
    pub fn begin_frame(&mut self) -> Result<Frame> { unsafe {
        let d = &self.device;

        // ① 等上一帧的 GPU 工作结束，命令缓冲才能重录
        d.wait_for_fences(&[self.in_flight], true, u64::MAX).map_err(vk_err("wait_for_fences"))?;

        // ② 向交换链要一张空闲图像。fence 放在 acquire 成功之后再重置：
        //    如果 acquire 失败就直接返回，fence 仍是已触发状态，下一帧不会卡死
        let (idx, _suboptimal) = self.swapchain_fn
            .acquire_next_image(self.swapchain, u64::MAX, self.image_ready, vk::Fence::null())
            .map_err(vk_err("acquire_next_image"))?;
        d.reset_fences(&[self.in_flight]).map_err(vk_err("reset_fences"))?;
        Ok(Frame { image_index: idx })
    }}

    /// 结束一帧：整屏清成透明，画出所有边框，提交并显示
    pub fn end_frame(&mut self, frame: Frame, outlines: &[Outline]) -> Result<()> { unsafe {
        let d = &self.device;
        let idx = frame.image_index;

        // ③ 录制命令
        d.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())
            .map_err(vk_err("reset_command_buffer"))?;
        d.begin_command_buffer(self.cmd, &vk::CommandBufferBeginInfo::default())
            .map_err(vk_err("begin_command_buffer"))?;

        let transparent = [vk::ClearValue { color: vk::ClearColorValue { float32: [0.0; 4] } }];
        d.cmd_begin_render_pass(
            self.cmd,
            &vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(self.framebuffers[idx as usize])
                .render_area(vk::Rect2D { offset: vk::Offset2D::default(), extent: self.extent })
                .clear_values(&transparent),
            vk::SubpassContents::INLINE,
        );
        for outline in outlines {
            self.record_outline(outline);
        }
        d.cmd_end_render_pass(self.cmd);
        d.end_command_buffer(self.cmd).map_err(vk_err("end_command_buffer"))?;

        // ④ 提交：等 image_ready 后才写颜色；完成后触发 render_done[idx] 和 in_flight
        let wait = [self.image_ready];
        let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let signal = [self.render_done[idx as usize]];
        let cmds = [self.cmd];
        let submit = [vk::SubmitInfo::default()
            .wait_semaphores(&wait)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal)];
        d.queue_submit(self.queue, &submit, self.in_flight).map_err(vk_err("queue_submit"))?;

        // ⑤ 显示：等 render_done[idx] 后把图像交给 SurfaceFlinger
        let swapchains = [self.swapchain];
        let indices = [idx];
        self.swapchain_fn.queue_present(
            self.queue,
            &vk::PresentInfoKHR::default()
                .wait_semaphores(&signal)
                .swapchains(&swapchains)
                .image_indices(&indices),
        ).map_err(vk_err("queue_present"))?;
        Ok(())
    }}

    /// 把一个边框拆成四条实心矩形，录制一条 vkCmdClearAttachments
    unsafe fn record_outline(&self, o: &Outline) {
        let [l, t, r, b] = o.rect;
        let th = o.thickness.max(1.0);

        // 先填充框内，再画边框，边框才不会被填充色盖住。
        // 注意 vkCmdClearAttachments 是直接写入像素、不做混合：重叠的框后画的会覆盖先画的，
        // 而不是两层半透明叠加变深。框之间很少重叠，这个差别基本看不出来
        if let Some(fill) = o.fill {
            if let Some(inner) = self.clip(l + th, t + th, r - th, b - th) {
                unsafe { self.device.cmd_clear_attachments(self.cmd, &[Self::clear_color(fill)], &[inner]); }
            }
        }

        let edges = [
            (l, t, r, t + th),      // 上
            (l, b - th, r, b),      // 下
            (l, t, l + th, b),      // 左
            (r - th, t, r, b),      // 右
        ];
        let rects: Vec<vk::ClearRect> = edges
            .iter()
            .filter_map(|&(x0, y0, x1, y1)| self.clip(x0, y0, x1, y1))
            .collect();
        if rects.is_empty() { return; }

        unsafe { self.device.cmd_clear_attachments(self.cmd, &[Self::clear_color(o.color)], &rects); }
    }

    /// 非预乘 RGBA → 清除到第 0 个颜色附件的参数。图层按预乘 alpha 混合，所以 RGB 要先乘以 alpha
    fn clear_color([r, g, b, a]: [f32; 4]) -> vk::ClearAttachment {
        vk::ClearAttachment {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            color_attachment: 0,
            clear_value: vk::ClearValue { color: vk::ClearColorValue { float32: [r * a, g * a, b * a, a] } },
        }
    }

    /// 浮点矩形 → 整数像素矩形，并裁剪到屏幕范围内
    ///
    /// vkCmdClearAttachments 要求每个矩形都完全落在 render area 里，超出屏幕的部分必须先裁掉；
    /// 完全在屏幕外或者宽高为 0 时返回 None
    fn clip(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Option<vk::ClearRect> {
        let (w, h) = (self.extent.width as f32, self.extent.height as f32);
        let x0 = x0.floor().clamp(0.0, w);
        let y0 = y0.floor().clamp(0.0, h);
        let x1 = x1.ceil().clamp(0.0, w);
        let y1 = y1.ceil().clamp(0.0, h);
        if x1 <= x0 || y1 <= y0 { return None; }
        Some(vk::ClearRect {
            rect: vk::Rect2D {
                offset: vk::Offset2D { x: x0 as i32, y: y0 as i32 },
                extent: vk::Extent2D { width: (x1 - x0) as u32, height: (y1 - y0) as u32 },
            },
            base_array_layer: 0,
            layer_count: 1,
        })
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            // 先等 GPU 把手上的活干完，否则销毁正在使用的对象是未定义行为
            let _ = self.device.device_wait_idle();
            let d = &self.device;
            d.destroy_fence(self.in_flight, None);
            for &s in &self.render_done { d.destroy_semaphore(s, None); }
            d.destroy_semaphore(self.image_ready, None);
            d.destroy_command_pool(self.pool, None);   // 连同从它分配的命令缓冲一起释放
            for &fb in &self.framebuffers { d.destroy_framebuffer(fb, None); }
            d.destroy_render_pass(self.render_pass, None);
            for &v in &self.views { d.destroy_image_view(v, None); }
            self.swapchain_fn.destroy_swapchain(self.swapchain, None);   // 交换链图像归它所有，不用单独销毁
            d.destroy_device(None);
            self.surface_fn.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}
