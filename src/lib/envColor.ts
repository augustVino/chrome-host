/** 环境渐变色：按 id 哈希确定性映射到 g1..g8（纯展示，不落库、不依赖后端字段） */
export function envGradient(id: string): number {
  let h = 0;
  for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) >>> 0;
  return (h % 8) + 1;
}

/** 环境首字母（支持中文/emoji） */
export function envInitial(name: string): string {
  return (Array.from(name.trim())[0] ?? '?').toUpperCase();
}
