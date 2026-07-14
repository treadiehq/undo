import {Composition} from 'remotion';
import {UndoLaunchVideo} from './UndoLaunchVideo';

export const RemotionRoot = () => {
  return (
    <Composition
      id="UndoLaunch"
      component={UndoLaunchVideo}
      durationInFrames={1260}
      fps={30}
      width={1920}
      height={1080}
    />
  );
};
