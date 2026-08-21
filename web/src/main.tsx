import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
// Astryx ships pre-built CSS, and the order is the layer cascade: the reset,
// then the components. The theme is defined in `theme.ts` and injected by the
// `Theme` provider at the root of the application.
import '@astryxdesign/core/reset.css'
import '@astryxdesign/core/astryx.css'
import './index.css'
import App from './App.tsx'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
