-- 0010_outbound_attachments.sql —— 出站消息带附件（C3.send 的附件那半边）。
--
-- 为什么新开文件而不是改 0002：已发布的迁移永不编辑（docs/05-quality-gates.md §2 G6）。
--
-- kind：本条出站是纯文本还是「带附件」。只有两个取值：
--   * text       —— 与 0009 之前的每一行完全一样（正文发送）；
--   * attachment —— 正文可以为空，真正的载荷是 attachment_json 指的文件。
--
-- 为什么图片/文件不再细分到这一列：那是领域概念，已经完整地写在 attachment_json 里的
-- `kind` 字段（`hbot_core::AttachmentRef`）。两处都写就有漂移风险，而列上的判据只需要
-- 「发不发文件」这一位 —— 适配器解析 JSON 拿具体分类。
-- 默认值 text 让老库里已有的行语义不变（ADD COLUMN NOT NULL 必须给默认值）。
ALTER TABLE outbound ADD COLUMN kind TEXT NOT NULL DEFAULT 'text';

-- 附件的描述（JSON：{"kind":"image","path":"C:\\proj\\assets\\logo.png"}）；NULL = 不带附件。
--
-- 存的是**入队那一刻解析好的绝对路径**，不是项目内相对路径：续发时项目可能已经换了根目录
--（projects.path 可变），拿相对路径重新解析会指向另一个文件。附件的语义是「发这个文件」，
-- 不是「发这个相对名字现在对应的文件」。路径只在本机库里，不外传给模型（坑 47 针对的是错误文案）。
ALTER TABLE outbound ADD COLUMN attachment_json TEXT;
