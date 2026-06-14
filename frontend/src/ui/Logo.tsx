import type { JSX } from 'preact';
import { useMemo } from 'preact/hooks';

let logoId = 0;

const LOGO_MARKUP = `
<defs>
    <!-- Poly 00 (Inner glowing core) - vertical gradient for vertical glow! -->
    <linearGradient id="grad_00" x1="0%" y1="0%" x2="0%" y2="100%">
      <stop offset="0%" stop-color="#8bbd25" />
      <stop offset="25%" stop-color="#bef134" />
      <stop offset="50%" stop-color="#f2ff7c" />
      <stop offset="75%" stop-color="#bef134" />
      <stop offset="100%" stop-color="#7da81e" />
    </linearGradient>
    
    <!-- Poly 01 (Top-right core step block) -->
    <linearGradient id="grad_01" x1="100%" y1="0%" x2="0%" y2="100%">
      <stop offset="0%" stop-color="#91c726" />
      <stop offset="100%" stop-color="#aae231" />
    </linearGradient>
    
    <!-- Poly 02 (Top-right core block face) -->
    <linearGradient id="grad_02" x1="100%" y1="100%" x2="0%" y2="0%">
      <stop offset="0%" stop-color="#89be43" />
      <stop offset="100%" stop-color="#9cd450" />
    </linearGradient>
    
    <!-- Poly 03 (The main outer face) -->
    <linearGradient id="grad_03" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#1e341f" />
      <stop offset="45%" stop-color="#497033" />
      <stop offset="75%" stop-color="#76ac27" />
      <stop offset="100%" stop-color="#9cd44c" />
    </linearGradient>
    
    <!-- Poly 04 (Top bevel triangle) -->
    <linearGradient id="grad_04" x1="0%" y1="0%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#3f6a45" />
      <stop offset="100%" stop-color="#4e7954" />
    </linearGradient>
    
    <!-- Poly 05 (Top-left beveled outer face) -->
    <linearGradient id="grad_05" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#1a3424" />
      <stop offset="100%" stop-color="#5a8649" />
    </linearGradient>
    
    <!-- Poly 06 (Left-most vertical face) -->
    <linearGradient id="grad_06" x1="0%" y1="100%" x2="0%" y2="0%">
      <stop offset="0%" stop-color="#143021" />
      <stop offset="100%" stop-color="#1f3c2e" />
    </linearGradient>
    
    <!-- Poly 07 (Bottom-left beveled outer face) -->
    <linearGradient id="grad_07" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#0c1a11" />
      <stop offset="100%" stop-color="#36562a" />
    </linearGradient>
    
    <!-- Poly 08 (Bottom bevel triangle) -->
    <linearGradient id="grad_08" x1="0%" y1="0%" x2="100%" y2="100%">
      <stop offset="0%" stop-color="#0e1a10" />
      <stop offset="100%" stop-color="#142317" />
    </linearGradient>
    
    <!-- Poly 09 (Bottom-right core step block) -->
    <linearGradient id="grad_09" x1="100%" y1="100%" x2="0%" y2="0%">
      <stop offset="0%" stop-color="#82b621" />
      <stop offset="100%" stop-color="#cffe3b" />
    </linearGradient>
    
    <!-- Poly 10 (Bottom-right core block face) -->
    <linearGradient id="grad_10" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#63991a" />
      <stop offset="100%" stop-color="#7fb328" />
    </linearGradient>
    
    <!-- Poly 11 -->
    <linearGradient id="grad_11" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#80b61c" /><stop offset="100%" stop-color="#94ce24" />
    </linearGradient>
    <!-- Poly 12 -->
    <linearGradient id="grad_12" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#77b120" /><stop offset="100%" stop-color="#89c42a" />
    </linearGradient>
    <!-- Poly 13 -->
    <linearGradient id="grad_13" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#81b70f" /><stop offset="100%" stop-color="#96d01d" />
    </linearGradient>
    <!-- Poly 14 -->
    <linearGradient id="grad_14" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#9bd942" /><stop offset="100%" stop-color="#b0ec4c" />
    </linearGradient>
    <!-- Poly 15 -->
    <linearGradient id="grad_15" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#79b10d" /><stop offset="100%" stop-color="#84be15" />
    </linearGradient>
    <!-- Poly 16 -->
    <linearGradient id="grad_16" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#598d2b" /><stop offset="100%" stop-color="#629831" />
    </linearGradient>
    <!-- Poly 17 -->
    <linearGradient id="grad_17" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#315a13" /><stop offset="100%" stop-color="#3a651b" />
    </linearGradient>
    <!-- Poly 18 -->
    <linearGradient id="grad_18" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#97d441" /><stop offset="100%" stop-color="#a6e54c" />
    </linearGradient>
    <!-- Poly 19 -->
    <linearGradient id="grad_19" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#5e8f0b" /><stop offset="100%" stop-color="#6a9c15" />
    </linearGradient>
    <!-- Poly 20 -->
    <linearGradient id="grad_20" x1="0%" y1="100%" x2="100%" y2="0%">
      <stop offset="0%" stop-color="#9cd254" /><stop offset="100%" stop-color="#ace45c" />
    </linearGradient>
  </defs>
  
  <g>
    <!-- Polygons -->
    <polygon points="325.00,500.00 325.00,400.00 450.00,275.00 575.00,275.00 575.00,250.00 550.00,250.00 550.00,200.00 400.00,200.00 325.00,275.00 325.00,325.00 275.00,325.00 275.00,500.00 275.00,675.00 325.00,675.00 325.00,725.00 400.00,800.00 550.00,800.00 550.00,750.00 575.00,750.00 575.00,725.00 450.00,725.00 325.00,600.00 325.00,500.00" fill="url(#grad_00)" stroke="url(#grad_00)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="575.00,275.00 575.00,325.00 650.00,325.00 650.00,250.00 575.00,250.00 575.00,275.00" fill="url(#grad_01)" stroke="url(#grad_01)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="650.00,250.00 750.00,250.00 750.00,175.00 650.00,175.00 650.00,250.00" fill="url(#grad_02)" stroke="url(#grad_02)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="550.00,200.00 600.00,200.00 600.00,100.00 650.00,100.00 650.00,25.00 400.00,25.00 400.00,100.00 325.00,100.00 275.00,100.00 275.00,275.00 200.00,275.00 200.00,400.00 125.00,400.00 125.00,500.00 125.00,600.00 200.00,600.00 200.00,725.00 275.00,725.00 275.00,900.00 325.00,900.00 400.00,900.00 400.00,975.00 650.00,975.00 650.00,900.00 600.00,900.00 600.00,800.00 550.00,800.00 400.00,800.00 325.00,725.00 325.00,675.00 275.00,675.00 275.00,500.00 275.00,325.00 325.00,325.00 325.00,275.00 400.00,200.00 550.00,200.00" fill="url(#grad_03)" stroke="url(#grad_03)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="400.00,100.00 400.00,25.00 325.00,100.00 400.00,100.00" fill="url(#grad_04)" stroke="url(#grad_04)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="275.00,275.00 275.00,100.00 125.00,250.00 125.00,350.00 75.00,400.00 125.00,400.00 200.00,400.00 200.00,275.00 275.00,275.00" fill="url(#grad_05)" stroke="url(#grad_05)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="125.00,500.00 125.00,400.00 75.00,400.00 75.00,500.00 75.00,600.00 125.00,600.00 125.00,500.00" fill="url(#grad_06)" stroke="url(#grad_06)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="200.00,600.00 125.00,600.00 75.00,600.00 125.00,650.00 125.00,750.00 275.00,900.00 275.00,725.00 200.00,725.00 200.00,600.00" fill="url(#grad_07)" stroke="url(#grad_07)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="400.00,900.00 325.00,900.00 400.00,975.00 400.00,900.00" fill="url(#grad_08)" stroke="url(#grad_08)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="575.00,750.00 650.00,750.00 650.00,675.00 575.00,675.00 575.00,725.00 575.00,750.00" fill="url(#grad_09)" stroke="url(#grad_09)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="650.00,750.00 650.00,825.00 750.00,825.00 750.00,750.00 650.00,750.00" fill="url(#grad_10)" stroke="url(#grad_10)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="775.00,650.00 725.00,650.00 725.00,700.00 775.00,700.00 775.00,650.00" fill="url(#grad_11)" stroke="url(#grad_11)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="725.00,300.00 725.00,350.00 775.00,350.00 775.00,300.00 725.00,300.00" fill="url(#grad_12)" stroke="url(#grad_12)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="875.00,725.00 825.00,725.00 825.00,800.00 875.00,800.00 875.00,725.00" fill="url(#grad_13)" stroke="url(#grad_13)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="825.00,200.00 825.00,275.00 875.00,275.00 875.00,200.00 825.00,200.00" fill="url(#grad_14)" stroke="url(#grad_14)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="925.00,675.00 900.00,675.00 900.00,700.00 925.00,700.00 925.00,675.00" fill="url(#grad_15)" stroke="url(#grad_15)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="900.00,300.00 900.00,325.00 925.00,325.00 925.00,300.00 900.00,300.00" fill="url(#grad_16)" stroke="url(#grad_16)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="900.00,825.00 875.00,825.00 875.00,850.00 900.00,850.00 900.00,825.00" fill="url(#grad_17)" stroke="url(#grad_17)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="875.00,150.00 875.00,175.00 900.00,175.00 900.00,150.00 875.00,150.00" fill="url(#grad_18)" stroke="url(#grad_18)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="800.00,875.00 750.00,875.00 750.00,925.00 800.00,925.00 800.00,875.00" fill="url(#grad_19)" stroke="url(#grad_19)" stroke-width="0.7" stroke-linejoin="round" />
    <polygon points="750.00,75.00 750.00,125.00 800.00,125.00 800.00,75.00 750.00,75.00" fill="url(#grad_20)" stroke="url(#grad_20)" stroke-width="0.7" stroke-linejoin="round" />
    
    <!-- 3. Specular inner curve edge highlights -->
    <path d="M550.00,200.00 L400.00,200.00 L325.00,275.00 L325.00,325.00 L275.00,325.00 L275.00,500.00 L275.00,675.00 L325.00,675.00 L325.00,725.00 L400.00,800.00 L550.00,800.00" fill="none" stroke="#fbfebd" stroke-width="2.5" stroke-opacity="0.45" stroke-linecap="round" stroke-linejoin="round" />
    
    <!-- Specular highlights for opening cuts -->
    <line x1="575.0" y1="250.0" x2="575.0" y2="275.0" stroke="#fbfebd" stroke-width="2.0" stroke-opacity="0.38" stroke-linecap="round" />
    <line x1="575.0" y1="725.0" x2="575.0" y2="750.0" stroke="#fbfebd" stroke-width="2.0" stroke-opacity="0.38" stroke-linecap="round" />
  </g>
`;

function scopedLogoMarkup(scope: string): string {
  return LOGO_MARKUP.replace(/id="([^"]+)"/g, `id="${scope}-$1"`).replace(/url\(#([^)]+)\)/g, `url(#${scope}-$1)`);
}

export function Logo({
  className,
  size = 26,
  style,
}: {
  className?: string;
  size?: number;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const markup = useMemo(() => {
    logoId += 1;
    return scopedLogoMarkup(`cp-logo-${logoId}`);
  }, []);

  return (
    <svg
      class={className}
      width={size}
      height={size}
      viewBox="0 0 1000 1000"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
      focusable="false"
      style={{
        width: size,
        height: size,
        filter: 'var(--logo-filter, none)',
        ...style,
      }}
      dangerouslySetInnerHTML={{ __html: markup }}
    />
  );
}
