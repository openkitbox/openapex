# skills

本目录是项目 skill 的版本追踪源。ox 从 `<项目>/.ox/skills` 加载项目 skill，而 `.ox` 被全局 gitignore 忽略，所以 `.ox/skills` 是指向这里的软链接。

新克隆仓库后执行一次：

```
mkdir -p .ox && ln -s ../skills .ox/skills
```

每个 skill 一个子目录，入口为 `SKILL.md`。
