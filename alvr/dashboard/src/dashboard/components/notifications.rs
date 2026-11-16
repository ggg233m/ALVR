use alvr_common::{LogEntry, LogSeverity};
use alvr_gui_common::theme::{self, log_colors};
use alvr_session::Settings;
use eframe::{
    egui::{self, Frame, Label, Layout, Panel, RichText, TextWrapMode, Ui},
    emath::Align,
    epaint::Color32,
};
use rand::seq::IndexedRandom;
use std::time::Duration;

#[cfg(target_arch = "wasm32")]
use instant::Instant;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

const TIMEOUT: Duration = Duration::from_secs(5);
const NO_NOTIFICATIONS_MESSAGE: &str = "无新通知";
const NOTIFICATION_TIPS: &[&str] = &[
    // The following tips are ordered roughtly in the order settings appear
    r#"如果修改某些设置后开始崩溃，请从“安装”选项卡重新运行“运行设置向导”，然后点击“重置设置”来重置 ALVR。"#,
    r#"部分设置默认隐藏。点击某些设置旁边的“展开”按钮可展开子菜单。"#,
    r#"强烈建议将 ALVR 中的音频设置保持默认，改为在任务栏托盘中修改默认音频设备。"#,
    r#"增大“视频”->“最大缓冲”可能减少卡顿，但会增加延迟。"#,
    r#"在某些 GPU 上，有时需要在 H264 和 HEVC 编解码器之间切换，以修复崩溃或回退到软件编码的问题。"#,
    r#"如果使用 NVIDIA GPU，最好使用高比特率 H264；如果使用 AMD GPU，HEVC 画质可能更好。"#,
    r#"如果出现“雪花”闪烁，请将“预设”->“分辨率”设为“低”，并禁用“视频”->“注视点编码”。"#,
    r#"增大“视频”->“色彩校正”->“锐度”可能提升观感画质。"#,
    r#"如果外部控制器或追踪器无法与 ALVR 追踪空间同步，可在“头显”->“额外 OpenVR 属性”中添加一项，然后设置自定义的“追踪系统名称字符串”。"#,
    r#"要更改控制器的外观，请设置“头显”->“控制器”->“仿真模式”。"#,
    r#"ALVR 支持自定义按键绑定！如需帮助，请在我们的 Discord 服务器上提问。"#,
    r#"ALVR 支持手部追踪手势（“预设”->“手部追踪交互”->“ALVR 绑定”）。正确用法请查看 wiki：https://github.com/alvr-org/ALVR/wiki/Hand-tracking-controller-bindings。"#,
    r#"如果手部追踪手势造成困扰，可在“头显”->“控制器”->“手部追踪交互”中禁用，或启用“手部追踪交互”->“仅触摸”。"#,
    r#"可通过“头显”->“控制器”->“预测”微调控制器的响应速度。"#,
    r#"如果可视的控制器/手部模型与实体控制器位置不符，可在“头显”->“控制器”->“左控制器位置/旋转偏移”中调整偏移（对两个控制器都生效）。"#,
    r#"使用外部追踪器或控制器时，应将“头显”->“位置/旋转重居中模式”都设为“禁用”。"#,
    r#"可启用倾斜模式：将“头显”->“位置重居中模式”设为“本地”，将“头显”->“旋转重居中模式”设为“倾斜”。"#,
    r#"如果经常出现画面故障，可通过“连接”->“避免视频画面故障”用卡顿帧来换取改善。"#,
    r#"可通过“连接”->“启用连接/断开脚本”在头显连接/断开时运行自定义命令/程序。"#,
    r#"若要报告问题并获取日志文件，请启用“额外”->“日志”->“记录到磁盘”。日志位于 "session_log.txt" 中。"#,
    r#"出于折腾目的，可启用“额外”->“日志”->“记录追踪”、“记录按键”和“记录触觉反馈”。可通过 ws://localhost:8082/api/events 的 websocket 获取数据。"#,
    r#"若要报告问题并分享日志，应启用“额外”->“日志”->“首选回溯”。"#,
    r#"切换“额外”->“日志”->“显示通知提示”即可快速轮换此类提示。"#,
    r#"启用“额外”->“SteamVR 启动器”->“自动打开和关闭 SteamVR”会很方便。"#,
    r#"若要分享录像以报告问题，可启用“额外”->“捕获”->“滚动视频文件”来限制上传文件大小。"#,
    // Miscellaneous
    r#"如果头显未出现在设备列表中，可能处于不同子网。请尝试用设备内显示的 IP“手动添加设备”。"#,
];

pub struct NotificationBar {
    message: String,
    current_level: LogSeverity,
    receive_instant: Instant,
    min_notification_level: LogSeverity,
    tip_message: Option<String>,
    expanded: bool,
}

impl NotificationBar {
    pub fn new() -> Self {
        Self {
            message: NO_NOTIFICATIONS_MESSAGE.into(),
            current_level: LogSeverity::Debug,
            receive_instant: Instant::now(),
            min_notification_level: LogSeverity::Debug,
            tip_message: None,
            expanded: false,
        }
    }

    pub fn update_settings(&mut self, settings: &Settings) {
        self.min_notification_level = settings.extra.logging.notification_level;

        if settings.extra.logging.show_notification_tip {
            if self.tip_message.is_none() {
                self.tip_message = NOTIFICATION_TIPS
                    .choose(&mut rand::rng())
                    .map(|s| format!("提示：{s}"));
            }
        } else {
            self.tip_message = None;
        }
    }

    pub fn push_notification(&mut self, event: LogEntry, from_dashboard: bool) {
        let now = Instant::now();
        let min_severity = if from_dashboard {
            if cfg!(debug_assertions) {
                LogSeverity::Debug
            } else {
                LogSeverity::Info
            }
        } else {
            self.min_notification_level
        };

        if event.severity >= min_severity
            && (now > self.receive_instant + TIMEOUT || event.severity >= self.current_level)
        {
            self.message = event.content;
            self.current_level = event.severity;
            self.receive_instant = now;
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let now = Instant::now();
        if now > self.receive_instant + TIMEOUT {
            self.message = self
                .tip_message
                .clone()
                .unwrap_or_else(|| NO_NOTIFICATIONS_MESSAGE.into());
            self.current_level = LogSeverity::Debug;
        }

        let (fg, bg) = match self.current_level {
            LogSeverity::Error => (Color32::BLACK, log_colors::ERROR_LIGHT),
            LogSeverity::Warning => (Color32::BLACK, log_colors::WARNING_LIGHT),
            LogSeverity::Info => (Color32::BLACK, log_colors::INFO_LIGHT),
            LogSeverity::Debug => (theme::FG, theme::LIGHTER_BG),
        };

        let mut bottom_bar = Panel::bottom("bottom_panel").frame(
            Frame::default()
                .inner_margin(egui::vec2(10.0, 5.0))
                .fill(bg),
        );
        let alignment = if !self.expanded {
            bottom_bar = bottom_bar.max_size(26.0);

            Align::TOP
        } else {
            Align::Center
        };
        let wrapping = if !self.expanded {
            TextWrapMode::Truncate
        } else {
            TextWrapMode::Wrap
        };

        bottom_bar.show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(alignment), |ui| {
                if !self.expanded {
                    if ui.small_button("展开").clicked() {
                        self.expanded = true;
                    }
                } else if ui.button("收起").clicked() {
                    self.expanded = false;
                }
                ui.with_layout(Layout::left_to_right(alignment), |ui| {
                    //A LayoutJob that has its TextWrapping updated to fill the available space would probably be a more elegant solution.
                    ui.add(
                        Label::new(RichText::new(&self.message).color(fg).size(12.0))
                            .wrap_mode(wrapping),
                    );
                })
            })
        });
    }
}
