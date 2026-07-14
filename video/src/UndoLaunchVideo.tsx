import type {CSSProperties, ReactNode} from 'react';
import {
  AbsoluteFill,
  Easing,
  Html5Audio,
  Img,
  interpolate,
  Sequence,
  spring,
  staticFile,
  useCurrentFrame,
  useVideoConfig,
} from 'remotion';

const FPS = 30;

const clamp = {
  extrapolateLeft: 'clamp' as const,
  extrapolateRight: 'clamp' as const,
};

const sceneOpacity = (frame: number, duration: number) =>
  interpolate(frame, [0, 14, duration - 14, duration], [0, 1, 1, 0], {
    ...clamp,
    easing: Easing.inOut(Easing.ease),
  });

const enter = (
  frame: number,
  delay = 0,
  distance = 34,
): CSSProperties => {
  const progress = spring({
    frame: frame - delay,
    fps: FPS,
    config: {damping: 18, stiffness: 100, mass: 0.85},
  });

  return {
    opacity: progress,
    transform: `translateY(${(1 - progress) * distance}px) scale(${0.985 + progress * 0.015})`,
  };
};

const Brand = ({large = false}: {large?: boolean}) => (
  <div className={`editorial-brand ${large ? 'editorial-brand-large' : ''}`}>
    <Img src={staticFile('undo-logo.png')} />
    <span>Undo</span>
  </div>
);

const GradientStage = ({
  children,
  duration,
  tone = 'default',
}: {
  children: ReactNode;
  duration: number;
  tone?: 'default' | 'green' | 'rose';
}) => {
  const frame = useCurrentFrame();
  const drift = interpolate(frame, [0, duration], [-2, 2], clamp);

  return (
    <AbsoluteFill
      className={`gradient-stage gradient-${tone}`}
      style={{
        opacity: sceneOpacity(frame, duration),
        transform: `scale(1.02) translateX(${drift}%)`,
      }}
    >
      <div className="editorial-grain" />
      {children}
    </AbsoluteFill>
  );
};

const SceneBrand = () => (
  <div className="scene-brand">
    <Brand />
  </div>
);

const BigStatement = ({
  children,
  subline,
  duration,
  tone = 'default',
}: {
  children: ReactNode;
  subline?: string;
  duration: number;
  tone?: 'default' | 'green' | 'rose';
}) => {
  const frame = useCurrentFrame();

  return (
    <GradientStage duration={duration} tone={tone}>
      <SceneBrand />
      <div className="statement-wrap">
        <h1 style={enter(frame, 5, 46)}>{children}</h1>
        {subline ? (
          <p style={enter(frame, 18, 24)}>{subline}</p>
        ) : null}
      </div>
    </GradientStage>
  );
};

const Cursor = () => <span className="editorial-cursor" />;

const TypingCommand = ({
  text,
  frame,
  start = 12,
  speed = 1.65,
}: {
  text: string;
  frame: number;
  start?: number;
  speed?: number;
}) => {
  const count = Math.max(
    0,
    Math.min(text.length, Math.floor((frame - start) * speed)),
  );

  return (
    <div className="typing-command">
      <span className="prompt">$</span>
      <span>{text.slice(0, count)}</span>
      {frame >= start && count < text.length ? <Cursor /> : null}
    </div>
  );
};

const Window = ({
  children,
  title = '~/project',
  style,
  centered = true,
}: {
  children: ReactNode;
  title?: string;
  style?: CSSProperties;
  centered?: boolean;
}) => {
  const {transform, ...rest} = style ?? {};

  return (
  <div
    className="editorial-window"
    style={{
      ...rest,
      transform: `${centered ? 'translateX(-50%) ' : ''}${transform ?? ''}`.trim(),
    }}
  >
    <div className="window-topbar">
      <div className="window-dots">
        <span />
        <span />
        <span />
      </div>
      <span>{title}</span>
      <span className="window-local">local</span>
    </div>
    {children}
  </div>
  );
};

const HookScene = () => {
  const frame = useCurrentFrame();
  const duration = 120;

  return (
    <GradientStage duration={duration}>
      <div className="hook-brand" style={enter(frame, 2, 20)}>
        <Brand large />
      </div>
      <div className="hook-statement">
        <h1 style={enter(frame, 10, 52)}>
          Undo the bad.
          <br />
          <span>Keep the good.</span>
        </h1>
      </div>
      <div className="hook-meta" style={enter(frame, 27, 18)}>
        File recovery for coding-agent work
      </div>
    </GradientStage>
  );
};

const RecordScene = () => {
  const frame = useCurrentFrame();
  const duration = 165;
  const command = 'undo run claude';
  const commandDoneAt = 12 + Math.ceil(command.length / 1.65);

  return (
    <GradientStage duration={duration}>
      <SceneBrand />
      <div className="scene-label" style={enter(frame, 4, 18)}>
        Record the run.
      </div>
      <Window style={enter(frame, 12, 56)}>
        <div className="window-terminal">
          <TypingCommand text={command} frame={frame} />
          <div className="terminal-result">
            {[
              ['Recording started for Run', 'r_421'],
              ['Claude Code', 'working…'],
              ['Run completed', '2m 14s'],
            ].map(([label, value], index) => {
              const show = interpolate(
                frame,
                [commandDoneAt + 12 + index * 18, commandDoneAt + 20 + index * 18],
                [0, 1],
                clamp,
              );

              return (
                <div
                  className="result-row"
                  key={label}
                  style={{
                    opacity: show,
                    transform: `translateY(${(1 - show) * 8}px)`,
                  }}
                >
                  <span className={index === 1 ? 'dot-active' : 'dot-done'} />
                  <span>{label}</span>
                  <strong>{value}</strong>
                </div>
              );
            })}
          </div>
        </div>
      </Window>
    </GradientStage>
  );
};

const WorkCard = ({
  label,
  title,
  path,
  value,
  good,
  frame,
  delay,
}: {
  label: string;
  title: string;
  path: string;
  value: string;
  good: boolean;
  frame: number;
  delay: number;
}) => (
  <div
    className={`work-card ${good ? 'work-good' : 'work-bad'}`}
    style={enter(frame, delay, 46)}
  >
    <div className="work-card-label">
      <span>{good ? '✓' : '!'}</span>
      {label}
    </div>
    <h3>{title}</h3>
    <div className="code-value">
      <small>{path}</small>
      <code>{value}</code>
    </div>
  </div>
);

const TwoThingsScene = () => {
  const frame = useCurrentFrame();
  const duration = 165;

  return (
    <GradientStage duration={duration}>
      <SceneBrand />
      <div className="two-things-heading" style={enter(frame, 4, 22)}>
        The agent did two things.
      </div>
      <div className="work-grid">
        <WorkCard
          label="KEEP"
          title="Realtime dashboard"
          path="dashboard/panel.conf"
          value="title=Realtime Dashboard"
          good
          frame={frame}
          delay={14}
        />
        <WorkCard
          label="REMOVE"
          title="Broken auth migration"
          path="auth/login.conf"
          value="provider=broken-oauth"
          good={false}
          frame={frame}
          delay={24}
        />
      </div>
    </GradientStage>
  );
};

const PreviewScene = () => {
  const frame = useCurrentFrame();
  const duration = 225;
  const command = 'undo ask r_421 "remove auth but keep dashboard"';
  const commandDoneAt = 15 + Math.ceil(command.length / 2.25);

  const rows = [
    {kind: 'undo', label: 'Would undo', value: 'app/auth'},
    {kind: 'keep', label: 'Would keep', value: 'app/dashboard'},
  ];

  return (
    <GradientStage duration={duration}>
      <SceneBrand />
      <div className="scene-label scene-label-small" style={enter(frame, 3, 18)}>
        Say what should go.
      </div>
      <Window style={enter(frame, 10, 54)}>
        <div className="preview-body">
          <TypingCommand
            text={command}
            frame={frame}
            start={15}
            speed={2.25}
          />
          <div className="preview-rows">
            {rows.map((row, index) => {
              const progress = spring({
                frame: frame - commandDoneAt - 14 - index * 15,
                fps: FPS,
                config: {damping: 18, stiffness: 120},
              });

              return (
                <div
                  className={`preview-row preview-${row.kind}`}
                  key={row.label}
                  style={{
                    opacity: progress,
                    transform: `translateY(${(1 - progress) * 14}px)`,
                  }}
                >
                  <span>{row.kind === 'keep' ? '✓' : '↺'}</span>
                  <strong>{row.label}</strong>
                  <code>{row.value}</code>
                  <small>1 file</small>
                </div>
              );
            })}
          </div>
          <div
            className="no-change"
            style={enter(frame, commandDoneAt + 52, 18)}
          >
            <span>✓</span>
            No files changed.
          </div>
        </div>
      </Window>
    </GradientStage>
  );
};

const ApplyScene = () => {
  const frame = useCurrentFrame();
  const duration = 150;
  const progress = spring({
    frame: frame - 56,
    fps: FPS,
    config: {damping: 15, stiffness: 105, mass: 0.85},
  });

  return (
    <GradientStage duration={duration} tone="green">
      <SceneBrand />
      <div className="apply-layout">
        <div className="apply-copy">
          <span style={enter(frame, 2, 18)}>Apply the exact plan.</span>
          <h2 style={enter(frame, 8, 34)}>
            Auth restored.
            <br />
            Dashboard untouched.
          </h2>
        </div>
        <Window centered={false} style={enter(frame, 15, 48)}>
          <div className="apply-terminal">
            <TypingCommand
              text="undo apply rec_812"
              frame={frame}
              start={20}
              speed={2}
            />
            <div
              className="apply-success"
              style={{
                opacity: progress,
                transform: `scale(${0.95 + progress * 0.05})`,
              }}
            >
              <span>✓</span>
              Changed 1 file
            </div>
            <div className="apply-proof" style={{opacity: progress}}>
              <code>provider=password</code>
              <small>title=Realtime Dashboard</small>
            </div>
          </div>
        </Window>
      </div>
    </GradientStage>
  );
};

const EndScene = () => {
  const frame = useCurrentFrame();
  const duration = 150;
  const {fps} = useVideoConfig();
  const logo = spring({
    frame: frame - 3,
    fps,
    config: {damping: 15, stiffness: 90, mass: 0.9},
  });
  const install = 'curl -fsSL https://useundo.co/install.sh | bash';
  const chars = Math.max(
    0,
    Math.min(install.length, Math.floor((frame - 44) * 1.55)),
  );

  return (
    <GradientStage duration={duration}>
      <div className="end-card">
        <div
          className="end-brand"
          style={{
            opacity: logo,
            transform: `scale(${0.86 + logo * 0.14})`,
          }}
        >
          <Brand large />
        </div>
        <h2 style={enter(frame, 16, 28)}>Give your agents an undo button.</h2>
        <div className="end-command" style={enter(frame, 35, 24)}>
          <span>$</span>
          {install.slice(0, chars)}
          {chars < install.length && frame >= 44 ? <Cursor /> : null}
        </div>
        <div className="end-foot" style={enter(frame, 77, 16)}>
          macOS + Linux
          <i />
          no account
          <i />
          everything stays local
        </div>
      </div>
    </GradientStage>
  );
};

export const UndoLaunchVideo = () => {
  return (
    <AbsoluteFill className="editorial-root">
      <Html5Audio src={staticFile('undo-score.wav')} volume={0.68} />

      <Sequence from={0} durationInFrames={120}>
        <HookScene />
      </Sequence>
      <Sequence from={105} durationInFrames={165}>
        <RecordScene />
      </Sequence>
      <Sequence from={255} durationInFrames={165}>
        <TwoThingsScene />
      </Sequence>
      <Sequence from={405} durationInFrames={105}>
        <BigStatement duration={105} tone="green">
          Keep the good.
        </BigStatement>
      </Sequence>
      <Sequence from={495} durationInFrames={105}>
        <BigStatement duration={105} tone="rose">
          Undo the bad.
        </BigStatement>
      </Sequence>
      <Sequence from={585} durationInFrames={225}>
        <PreviewScene />
      </Sequence>
      <Sequence from={795} durationInFrames={105}>
        <BigStatement duration={105} subline="Review the result. Change nothing.">
          Preview first.
        </BigStatement>
      </Sequence>
      <Sequence from={885} durationInFrames={105}>
        <BigStatement duration={105} subline="No account. No cloud service.">
          Stays local.
        </BigStatement>
      </Sequence>
      <Sequence from={975} durationInFrames={150}>
        <ApplyScene />
      </Sequence>
      <Sequence from={1110} durationInFrames={150}>
        <EndScene />
      </Sequence>

      <div className="editorial-frame" />
    </AbsoluteFill>
  );
};
