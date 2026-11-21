import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './app/App'
import { readPrefs } from './lib/layout-prefs'
import { applyTheme } from './lib/theme'
import './styles/theme.css'
import './styles/app.css'
import './styles/mino.css'

// 首帧之前先把主题贴上：否则深色存档会先闪一下浅色再切过去。
applyTheme(readPrefs().theme)

const container = document.getElementById('root')
if (!container) throw new Error('找不到 #root 容器：index.html 被改动？')

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
)