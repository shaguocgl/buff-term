import Modal from './Modal';
import { StarIcon } from './Icons';

interface Props {
  /** 本机累计启动次数，用于感谢文案 */
  launches: number;
  /** 「去 GitHub Star」：打开项目页并永久关闭提示 */
  onStar: () => void;
  /** 「以后再说」：关闭并按规则延后若干次启动再提示（X / Esc / 点遮罩同效） */
  onLater: () => void;
  /** 「不再提示」：永久关闭 */
  onNever: () => void;
}

/**
 * 启动超过 5 次后的 GitHub Star 引导弹窗。
 * 注意关闭途径的语义：三个按钮各自持久化（去Star/以后再说/不再提示），
 * 而 X / Esc / 遮罩关闭按「以后再说」处理，避免误触导致永不提示。
 */
export default function StarPromptModal({
  launches,
  onStar,
  onLater,
  onNever,
}: Props) {
  return (
    <Modal title="感谢使用 buffTerm" onClose={onLater} className="modal-sm">
      <div className="star-prompt">
        <p className="star-prompt-text">
          buffTerm 已经陪伴你完成了 {launches} 次启动。
          如果它对你有帮助，欢迎到 GitHub 项目点一颗 Star，
          这是对持续开发最好的鼓励。
        </p>
        <div className="tool-actions">
          <button className="btn primary small" onClick={onStar}>
            <StarIcon size={13} /> 去 GitHub Star
          </button>
          <button className="btn ghost small" onClick={onLater}>
            以后再说
          </button>
          <button className="btn ghost small" onClick={onNever}>
            不再提示
          </button>
        </div>
      </div>
    </Modal>
  );
}
