import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App.tsx'
import { restoreTheme } from './book/theme'

// Before the first paint, or the page flashes the other theme at a reader who
// already said which one they wanted.
restoreTheme()

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
