import * as React from "react";
import { LiquidMetal } from "@paper-design/shaders-react";

/** Catches WebGL/shader failures so the button never goes blank: the dark
 * core + glyph render outside the boundary and stand alone fine. */
class ShaderBoundary extends React.Component<
  { children: React.ReactNode },
  { failed: boolean }
> {
  state = { failed: false };
  static getDerivedStateFromError(): { failed: boolean } {
    return { failed: true };
  }
  componentDidCatch(): void {
    this.setState({ failed: true });
  }
  render(): React.ReactNode {
    return this.state.failed ? null : this.props.children;
  }
}

/** Liquid-metal play jewel: shader ring, dark core (matches the
 * near-black contract), light glyph. Glyph-only fallback if WebGL dies. */
export function LiquidPlayGlyph({
  glyph,
  repetition = 4,
}: {
  glyph: string;
  repetition?: number;
}): React.ReactNode {
  return (
    <>
      <ShaderBoundary>
        <span className="lm-shader" aria-hidden="true">
          <LiquidMetal
            colorBack="#3a4048"
            colorTint="#e8edf2"
            speed={0.5}
            repetition={repetition}
            softness={0.5}
            angle={45}
            scale={8}
            shape="none"
            style={{ width: "100%", height: "100%" }}
          />
        </span>
      </ShaderBoundary>
      <span className="lm-core" aria-hidden="true" />
      <span className="lm-glyph">{glyph}</span>
    </>
  );
}
