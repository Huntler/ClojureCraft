;; Evaluation harness for candidate transformation functions.
;;
;; Reads one EDN request from stdin:
;;   {:code "<source>" :fn "<public fn name>"
;;    :calls [{:input "<edn>" :expected "<edn>"} ...]   ; :expected is optional; #throws Class
;;                                                       ; expects an exception of that class
;;    :checks true :lint true
;;    :contract {:output {:type :map :keys [{:key "<edn>" :required true :type :integer :nil :disallowed}]}}
;;    :equality {:float-tolerance 1e-9 :unordered false}
;;    :call-ms 5000 :max-elements 1000000
;;    :output-chars 4000000                              ; optional: print results unbounded up to this length
;;    :probe-code "<source with probes>"                ; optional: record intermediate values
;;    :probe-failing-only true                           ; optional: instrumented runs only of failing calls
;;    :spread true                                       ; optional: each input is a vector of arguments
;;    :project {:checked ["f" "helper"] :namespaces ["my.util"]}}
;;                                                       ; optional: :code is a whole project namespace file;
;;                                                       ; only the :checked definitions are statically
;;                                                       ; checked and linted, and they may use :namespaces
;; and writes one JSON object per line to stdout as it goes:
;;   read, check, lint, load       the stages before any call
;;   start {index}                 a call begins (so a process that dies mid-call is attributable)
;;   result {index ...}            a call's outcome, comparison, and contract findings
;;   probe-setup, probe {index}    with :probe-code: the instrumented run of each call
;;   halted                        a call exceeded its time limit; the process stops without running the rest
;;   done                          everything ran
;;
;; The static checks below are a second line of defence that produce clear diagnostics. The
;; operating-system sandbox around this JVM is the security boundary.

(ns clojurecraft.harness
  (:require [clojure.edn :as edn]
            [clojure.string :as string]
            [clojure.walk :as walk])
  (:import (java.io StringWriter Writer)))

(def allowed-namespaces
  '#{clojure.core clojure.string clojure.set clojure.walk clojure.edn clojure.zip clojure.data})

;; Evaluation, reflection over namespaces and vars, state that escapes the call, concurrency,
;; class generation, and I/O. Java interop is allowed; the OS sandbox confines it.
(def denied-core
  '#{eval load load-file load-string load-reader read read-string read+string
     resolve ns-resolve requiring-resolve find-var intern var var-get var-set alter-var-root
     with-redefs with-redefs-fn binding with-bindings with-bindings* push-thread-bindings pop-thread-bindings
     set! ns-publics ns-interns ns-map ns-refers ns-aliases ns-imports all-ns find-ns the-ns
     create-ns remove-ns in-ns ns-unmap ns-unalias refer refer-clojure use require import alias
     future future-call pmap pcalls agent send send-off send-via await await-for shutdown-agents
     add-watch set-agent-send-executor! set-agent-send-off-executor! thread-bound?
     slurp spit file-seq line-seq with-open
     gen-class gen-interface proxy proxy-super reify deftype defrecord definterface
     extend extend-type extend-protocol monitor-enter monitor-exit locking
     set-validator! add-tap tap>  *in* with-in-str})

(def allowed-classes '#{Math StrictMath Long Integer Double Character Boolean})


(def allowed-top-level '#{defn defn- def declare})

;; Namespaces checked code may refer to; a project file adds the namespaces it requires.
(def ^:dynamic *usable-namespaces* allowed-namespaces)

;; ---------------------------------------------------------------------------------------------
;; Output

(def ^:private ^Writer raw-out *out*)

(defn- json-string [^String s]
  (let [sb (StringBuilder. (+ 2 (.length s)))]
    (.append sb \")
    (dotimes [i (.length s)]
      (let [c (.charAt s i)]
        (case c
          \" (.append sb "\\\"")
          \\ (.append sb "\\\\")
          \newline (.append sb "\\n")
          \return (.append sb "\\r")
          \tab (.append sb "\\t")
          (if (< (int c) 0x20)
            (.append sb (format "\\u%04x" (int c)))
            (.append sb c)))))
    (.append sb \")
    (.toString sb)))

(defn- json [x]
  (cond
    (nil? x) "null"
    (boolean? x) (str x)
    (string? x) (json-string x)
    (keyword? x) (json-string (name x))
    (integer? x) (str x)
    (number? x) (let [d (double x)]
                  (if (or (Double/isNaN d) (Double/isInfinite d)) "null" (str d)))
    (map? x) (str "{" (string/join "," (map (fn [[k v]] (str (json-string (name k)) ":" (json v))) x)) "}")
    (coll? x) (str "[" (string/join "," (map json x)) "]")
    :else (json-string (str x))))

(defn- emit [m]
  (.write raw-out (str (json m) "\n"))
  (.flush raw-out))

;; java.time values without an EDN form print as tagged literals that read back to equal values,
;; so they can be recorded, shown, and written as inputs and expectations.
;; An expected exception: the call must throw an instance of the named class (simple or full
;; name, a subclass included) instead of returning.
(defrecord Throws [class-name])

(defmethod print-method Throws [t ^Writer w]
  (.write w (str "#throws " (:class-name t))))

(def time-readers
  {'time/duration #(java.time.Duration/parse %)
   'time/date #(java.time.LocalDate/parse %)
   'time/date-time #(java.time.LocalDateTime/parse %)
   'time/time #(java.time.LocalTime/parse %)})

(defn read-value
  "One EDN value, with the java.time tags and #throws."
  [s]
  (edn/read-string {:readers (assoc time-readers 'throws #(->Throws (str %)))} s))

(defn- throws-match?
  "Whether `t` is an instance of the class named `class-name`."
  [class-name ^Throwable t]
  (loop [^Class c (class t)]
    (cond
      (nil? c) false
      (or (= class-name (.getName c)) (= class-name (.getSimpleName c))) true
      :else (recur (.getSuperclass c)))))

(doseq [[^Class c tag] [[java.time.Duration "time/duration"]
                        [java.time.LocalDate "time/date"]
                        [java.time.LocalDateTime "time/date-time"]
                        [java.time.LocalTime "time/time"]]]
  (.addMethod ^clojure.lang.MultiFn print-method c
              (fn [v ^Writer w] (.write w (str "#" tag " \"" v "\"")))))

;; java.time.Instant prints as #inst, so its values are recorded and shown as EDN; read back, an
;; #inst is a java.util.Date, which `difference` compares with an Instant by its milliseconds.
(defmethod print-method java.time.Instant [^java.time.Instant i ^Writer w]
  (.write w (str "#inst \"" i "\"")))

(defn- bounded-str
  "Prints `x` readably with collection lengths and nesting bounded, then at most `limit` chars."
  [x length level limit]
  (let [s (binding [*print-length* length *print-level* level *print-namespace-maps* false]
            (pr-str x))]
    (if (> (count s) limit) (str (subs s 0 limit) " …") s)))

(defn- show [x] (bounded-str x 12 5 240))

;; ---------------------------------------------------------------------------------------------
;; Static checks

(defn- violation [msg form]
  (throw (ex-info msg {:phase :check :form (pr-str form) :line (:line (meta form))})))

(defn- class-name?
  "A Java class (`Instant`, `java.time.Instant`) rather than a namespace: its last segment is
  capitalized."
  [s]
  (Character/isUpperCase (char (first (peek (string/split s #"\."))))))

(def ^:private let-heads
  '#{let let* loop loop* when-let if-let when-some if-some when-first for doseq dotimes with-open
     with-local-vars})

(def ^:private fn-heads '#{fn fn* defn defn- defmacro})

(defn- target-names
  "The symbols a binding target binds, destructuring included."
  [target]
  (->> (tree-seq coll? seq target)
       (filter #(and (symbol? %) (nil? (namespace %)) (not= '& %)))))

(defn- bound-names
  "Every name `form` binds as a local somewhere: function parameters and let-style bindings. Such
   a local may be called like a denied function (`read`, `send`, `var`) without being one."
  [form]
  (let [found (volatile! #{})
        bind! (fn [targets] (vswap! found into (mapcat target-names targets)))]
    (walk/prewalk
     (fn [x]
       (when (seq? x)
         (let [head (first x)]
           (cond
             (and (contains? let-heads head) (vector? (second x)))
             (doseq [[target value] (partition 2 (second x))]
               (if (= :let target)
                 (when (vector? value) (bind! (take-nth 2 value)))
                 (bind! [target])))

             (contains? fn-heads head)
             (doseq [part (rest x)]
               (cond
                 (vector? part) (bind! [part])
                 (and (seq? part) (vector? (first part))) (bind! [(first part)])))

             (and (= 'letfn head) (vector? (second x)))
             (doseq [spec (second x) :when (seq? spec)]
               (bind! [(first spec)])
               (doseq [part (rest spec)]
                 (cond
                   (vector? part) (bind! [part])
                   (and (seq? part) (vector? (first part))) (bind! [(first part)])))))))
       x)
     form)
    @found))

(def ^:dynamic ^:private *locals* #{})

(defn- check-symbol [aliases sym]
  (let [ns-part (namespace sym)
        name-part (name sym)]
    (cond
      ;; A special form stays one even where a local has its name.
      (and (nil? ns-part) (contains? denied-core sym)
           (or (special-symbol? sym) (not (contains? *locals* sym))))
      (violation (str "use of " sym " is not allowed") sym)

      (nil? ns-part) nil

      (and (not (contains? aliases (symbol ns-part))) (class-name? ns-part)) nil

      :else
      (let [target (get aliases (symbol ns-part) (symbol ns-part))]
        (cond
          (not (contains? *usable-namespaces* target))
          (violation (str "namespace or class " ns-part " is not allowed") sym)

          (and (= target 'clojure.core) (contains? denied-core (symbol name-part)))
          (violation (str "use of " sym " is not allowed") sym))))))

(defn- check-body* [aliases form]
  (walk/prewalk
   (fn [x]
     (cond
       (and (seq? x) (= 'quote (first x))) nil
       (symbol? x) (do (check-symbol aliases x) x)
       :else x))
   form))

(defn- check-body [aliases form]
  (binding [*locals* (bound-names form)]
    (check-body* aliases form)))

(defn- ns-aliases-of
  "Validates the ns form and returns {alias namespace}."
  [form]
  (when-not (and (seq? form) (= 'ns (first form)) (symbol? (second form)))
    (violation "the first form must be an ns declaration" form))
  (reduce
   (fn [aliases clause]
     (cond
       (string? clause) aliases
       (and (seq? clause) (= :require (first clause)))
       (reduce (fn [acc spec]
                 (let [[lib & opts] (if (vector? spec) spec [spec])
                       opts (apply hash-map opts)]
                   (when-not (contains? allowed-namespaces lib)
                     (violation (str "requiring " lib " is not allowed") spec))
                   (when (and (:refer opts) (not (vector? (:refer opts))))
                     (violation ":refer :all is not allowed; use an alias or a vector of names" spec))
                   (cond-> acc (:as opts) (assoc (:as opts) lib))))
               aliases (rest clause))
       (and (seq? clause) (= :import (first clause))) aliases
       :else (violation "only :require and :import clauses are allowed in ns" clause)))
   {} (drop 2 form)))

(defn read-all [code]
  (binding [*read-eval* false
            *default-data-reader-fn* nil]
    (let [rdr (clojure.lang.LineNumberingPushbackReader. (java.io.StringReader. code))
          eof (Object.)]
      (loop [forms []]
        (let [f (read {:eof eof :read-cond :allow :features #{:clj}} rdr)]
          (if (identical? f eof) forms (recur (conj forms f))))))))

(defn check-forms [forms]
  (let [aliases (ns-aliases-of (first forms))]
    (doseq [f (rest forms)]
      (when-not (and (seq? f) (contains? allowed-top-level (first f)))
        (violation "only defn, defn-, def and declare are allowed at top level" f))
      (check-body aliases f))
    (second (first forms))))

(defn- project-aliases
  "Validates a project file's ns form and returns {alias namespace}, whatever it requires."
  [form]
  (when-not (and (seq? form) (= 'ns (first form)) (symbol? (second form)))
    (violation "the first form must be an ns declaration" form))
  (let [options (fn [opts] (try (apply hash-map opts) (catch Throwable _ {})))
        add (fn [acc lib opts]
              (let [a (or (:as opts) (:as-alias opts))]
                (cond-> acc (symbol? a) (assoc a lib))))]
    (reduce
     (fn [aliases clause]
       (if (and (seq? clause) (= :require (first clause)))
         (reduce (fn [acc spec]
                   (cond
                     (vector? spec) (add acc (first spec) (options (rest spec)))
                     (seq? spec) (let [[prefix & specs] spec]
                                   (reduce (fn [acc s]
                                             (let [[lib & opts] (if (vector? s) s [s])]
                                               (add acc (symbol (str prefix "." lib)) (options opts))))
                                           acc specs))
                     :else acc))
                 aliases (rest clause))
         aliases))
     {} (drop 2 form))))

(defn check-project-forms
  "Checks only the definitions named in :checked; the rest of the file is the user's own code."
  [forms {:keys [checked namespaces]}]
  (let [aliases (project-aliases (first forms))
        checked (set (map symbol checked))]
    (binding [*usable-namespaces* (into allowed-namespaces (map symbol namespaces))]
      (doseq [f (rest forms)
              :when (and (seq? f) (contains? checked (second f)))]
        (when-not (contains? allowed-top-level (first f))
          (violation "only defn, defn-, def and declare are allowed at top level" f))
        (check-body aliases f)))
    (second (first forms))))

(defn- error-map [phase ^Throwable t]
  (let [data (ex-data t)
        root (loop [^Throwable c t]
               (if (and (instance? clojure.lang.Compiler$CompilerException c) (.getCause c))
                 (recur (.getCause c))
                 c))]
    {:phase (name (or (:phase data) phase))
     :class (.getName (class root))
     :message (str (.getMessage root))
     :line (or (:line data) (:clojure.error/line data))
     :form (:form data)}))

;; ---------------------------------------------------------------------------------------------
;; Lint: an analysis of the code after macroexpansion, made before it is evaluated. Only
;; clojure.core macros can be expanded (candidate code cannot define macros), so no candidate
;; code runs here.

(def ^:private special-forms
  '#{def loop* recur if case* let* letfn* do fn* quote var import* . set! deftype* reify* try
     throw monitor-enter monitor-exit catch finally new})

(def ^:private max-findings 40)

(defn- finding! [ctx level line msg]
  (when (< (count @(:findings ctx)) max-findings)
    (swap! (:findings ctx) conj {:level level :line line :message msg})))

(defn- ignorable-name? [sym]
  (let [n (name sym)]
    (or (.startsWith n "_") (.contains n "__") (= n "&"))))

(defn- line-of [form line]
  (or (when (instance? clojure.lang.IObj form) (:line (meta form))) line))

(declare analyze)

(defn- analyze-all [forms env ctx line]
  (doseq [f forms] (analyze f env ctx line)))

(defn- new-binding [sym line] {:sym sym :line line :used (volatile! false)})

(defn- warn-hidden
  "A local named like a definition of the namespace hides it in its scope."
  [ctx sym line]
  (when (and (symbol? sym) (not (ignorable-name? sym))
             (contains? (ns-interns (:ns ctx)) sym))
    (finding! ctx :warning line
              (str sym " hides " sym " of this namespace in this scope; give the local another name"))))

(defn- check-discarded
  "Forms written in the source before the last form of a body: in pure code their value is lost."
  [ctx body line]
  (doseq [f (butlast body)]
    (when (and (seq? f) (:line (meta f))
               (not (contains? '#{throw assert when when-not comment} (first f))))
      (finding! ctx :error (line-of f line)
                (str "the value of " (binding [*print-length* 3 *print-level* 3] (pr-str f))
                     " is thrown away: a let, fn, or do body returns only its last form."
                     " Check the parentheses, so that this form's value is used or returned."
                     " Clojure data is immutable: (assoc m k v) returns a new map and leaves m unchanged,"
                     " so apply optional changes in one expression, such as"
                     " (cond-> m has-a? (assoc :a a) has-b? (assoc :b b))")))))

(defn- warn-unused [ctx bindings]
  (doseq [{:keys [sym line used]} bindings]
    (when (and (not @used) (not (ignorable-name? sym)))
      (finding! ctx :warning line (str sym " is bound but never used")))))

(defn- analyze-bindings
  "Sequential bindings as in let* and loop*; returns the extended env and the new bindings."
  [pairs env ctx line]
  (loop [pairs (partition 2 pairs) env env acc []]
    (if-let [[sym init] (first pairs)]
      (do (analyze init env ctx line)
          (warn-hidden ctx sym (line-of sym line))
          (let [b (new-binding sym (line-of sym line))]
            (recur (rest pairs) (assoc env sym b) (conj acc b))))
      [env acc])))

(defn- analyze-fn-method [[params & body] env ctx line]
  (let [bs (mapv #(new-binding % line) (remove #{'&} params))
        _ (doseq [p (remove #{'&} params)] (warn-hidden ctx p line))
        env (into env (map (juxt :sym identity) bs))]
    (check-discarded ctx body line)
    (analyze-all body env (assoc ctx :in-fn true) line)
    (warn-unused ctx bs)))

(defn- analyze-fn [form env ctx line]
  (let [[_ & more] form
        [fname more] (if (symbol? (first more)) [(first more) (rest more)] [nil more])
        env (cond-> env fname (assoc fname (new-binding fname line)))
        methods (if (vector? (first more)) [more] more)]
    (doseq [m methods] (analyze-fn-method m env ctx line))))

(defn- arity-ok? [arglists n]
  (some (fn [params]
          (let [[fixed rest-args] (split-with #(not= '& %) params)]
            (if (seq rest-args) (>= n (count fixed)) (= n (count fixed)))))
        arglists))

(defn- check-arity [ctx ^clojure.lang.Var v n line]
  (let [arglists (:arglists (meta v))
        vns (str (.-ns v))]
    (when (and (seq? arglists) (every? vector? arglists)
               (or (.startsWith vns "clojure.") (= vns (str (:ns ctx))))
               (not (arity-ok? arglists n)))
      (finding! ctx :error line
                (str (.-sym v) " is called with " n (if (= 1 n) " argument" " arguments")
                     " but takes " (string/join " or " (map pr-str arglists)))))))

(defn- resolve-in [ctx sym]
  (try (ns-resolve (:ns ctx) sym) (catch Throwable _ nil)))

(defn- analyze-symbol [sym env ctx line]
  (if-let [b (get env sym)]
    (vreset! (:used b) true)
    (let [r (resolve-in ctx sym)]
      (cond
        (var? r) (swap! (:used-vars ctx) conj r)
        (class? r) nil
        (and (namespace sym) (class? (resolve-in ctx (symbol (namespace sym))))) nil
        (contains? special-forms sym) nil
        :else (finding! ctx :error line (str "Unable to resolve symbol: " sym))))))

(defn- analyze-case [[_ expr _shift _mask default imap] env ctx line]
  (analyze expr env ctx line)
  (analyze default env ctx line)
  (doseq [[_ [_test then]] imap] (analyze then env ctx line)))

(defn- analyze-try [[_ & body] env ctx line]
  (doseq [f body]
    (cond
      (and (seq? f) (= 'catch (first f)))
      (let [[_ _class sym & cbody] f
            b (new-binding sym line)]
        (analyze-all cbody (assoc env sym b) ctx line))
      (and (seq? f) (= 'finally (first f))) (analyze-all (rest f) env ctx line)
      :else (analyze f env ctx line))))

(defn- analyze-special [head form env ctx line]
  (case head
    quote nil
    var nil
    import* nil
    deftype* nil
    reify* nil
    def (do (when (:in-fn ctx)
              (finding! ctx :error line "def inside a function defines a global var on every call; use let"))
            (analyze-all (drop 2 form) env ctx line))
    (let* loop*) (let [[env bs] (analyze-bindings (second form) env ctx line)]
                   (check-discarded ctx (drop 2 form) line)
                   (analyze-all (drop 2 form) env ctx line)
                   (warn-unused ctx bs))
    letfn* (let [pairs (partition 2 (second form))
                 bs (mapv (fn [[sym _]] (new-binding sym line)) pairs)
                 env (into env (map (juxt :sym identity) bs))]
             (doseq [[_ f] pairs] (analyze f env ctx line))
             (check-discarded ctx (drop 2 form) line)
             (analyze-all (drop 2 form) env ctx line)
             (warn-unused ctx bs))
    fn* (analyze-fn form env ctx line)
    case* (analyze-case form env ctx line)
    try (analyze-try form env ctx line)
    . (let [[_ target member & args] form]
        (when-not (and (symbol? target) (class? (resolve-in ctx target)))
          (analyze target env ctx line))
        (if (seq? member)
          (analyze-all (rest member) env ctx line)
          (analyze-all args env ctx line)))
    new (analyze-all (drop 2 form) env ctx line)
    do (do (check-discarded ctx (rest form) line)
           (analyze-all (rest form) env ctx line))
    (analyze-all (rest form) env ctx line)))

(defn- analyze-call [form env ctx line]
  (let [head (first form)
        r (when (and (symbol? head) (not (contains? env head))) (resolve-in ctx head))]
    (if (and (var? r) (:macro (meta r)))
      (let [expanded (try
                       (binding [*ns* (:ns ctx)] (macroexpand-1 form))
                       (catch Throwable t
                         (finding! ctx :error line
                                   (str "invalid use of " head ": "
                                        (first (string/split-lines (str (.getMessage (or (.getCause t) t)))))))
                         ::invalid))]
        (swap! (:used-vars ctx) conj r)
        (when-not (= ::invalid expanded)
          (analyze expanded env ctx line)))
      (let [expanded (if (and (symbol? head) (nil? r) (not (contains? env head)))
                       (try (binding [*ns* (:ns ctx)] (macroexpand-1 form)) (catch Throwable _ form))
                       form)]
        (if (not (identical? expanded form))
          (analyze expanded env ctx line)
          (do (when (var? r) (check-arity ctx r (count (rest form)) line))
              (analyze-all form env ctx line)))))))

(defn analyze [form env ctx line]
  (let [line (line-of form line)]
    (cond
      (symbol? form) (analyze-symbol form env ctx line)
      (seq? form) (cond
                    (empty? form) nil
                    (and (symbol? (first form)) (contains? special-forms (first form))
                         (not (contains? env (first form))))
                    (analyze-special (first form) form env ctx line)
                    :else (analyze-call form env ctx line))
      (map? form) (doseq [[k v] form] (analyze k env ctx line) (analyze v env ctx line))
      (coll? form) (analyze-all form env ctx line)
      :else nil)))

(defn- declared-arglists
  "The parameter vectors of a defn form, or nil."
  [[_ _ & more]]
  (let [more (drop-while #(or (string? %) (map? %)) more)]
    (cond
      (vector? (first more)) (list (first more))
      (every? seq? more) (seq (map first (filter #(vector? (first %)) more))))))

(declare lint-project lint-fresh)

(defn lint
  "Findings for the forms after `ns`, analysed in namespace `ns-obj` before any of them is evaluated.
  With `only` (a set of names), only the definitions named are analysed."
  ([ns-obj forms] (lint ns-obj forms nil))
  ([ns-obj forms only]
   (if only
     (lint-project ns-obj forms only)
     (lint-fresh ns-obj forms))))

(defn- defined-names
  "The vars a top-level form defines, as far as that can be told without evaluating it."
  [f]
  (when (and (seq? f) (symbol? (first f)) (symbol? (second f)))
    (let [[head n & more] f]
      (cond
        (contains? '#{defrecord deftype} head) [(symbol (str "->" n)) (symbol (str "map->" n))]
        (= 'defprotocol head) (keep #(when (and (seq? %) (symbol? (first %))) (first %)) more)
        (= 'declare head) (filter symbol? (rest f))
        (.startsWith (name head) "def") [n]))))

(defn- lint-project
  "Lints only the definitions named in `only`, with every name the file defines resolvable."
  [ns-obj forms only]
  (let [ctx {:ns ns-obj :findings (atom []) :used-vars (atom #{})}
        mine (filter #(and (seq? %) (contains? only (second %))) forms)]
    (doseq [f forms
            n (defined-names f)]
      (try
        (let [v (intern ns-obj (with-meta n {}))]
          (when (contains? '#{defn defn-} (first f))
            (alter-meta! v assoc :arglists (declared-arglists f))))
        (catch Throwable _ nil)))
    (doseq [f mine]
      (try
        (analyze f {} ctx (line-of f nil))
        (catch Throwable t
          (finding! ctx :warning (line-of f nil)
                    (str "this form could not be fully analysed: " (.getMessage t))))))
    (doseq [f mine
            :when (= 'defn- (first f))
            :let [v (get (ns-interns ns-obj) (second f))]
            :when (and v (not (contains? @(:used-vars ctx) v)))]
      ;; A helper may also be used by code outside the checked definitions.
      (when-not (some #(and (seq? %) (not (contains? only (second %)))
                            (some (fn [x] (= x (second f))) (flatten (seq %))))
                      forms)
        (finding! ctx :warning (line-of f nil) (str "private function " (second f) " is never used"))))
    @(:findings ctx)))

(defn- lint-fresh [ns-obj forms]
  (let [ctx {:ns ns-obj :findings (atom []) :used-vars (atom #{})}
        private (atom [])]
    ;; Make every top-level name resolvable, with its declared parameters for arity checks.
    (doseq [f forms]
      (when (and (seq? f) (symbol? (second f)) (contains? '#{defn defn- def declare} (first f)))
        (let [names (if (= 'declare (first f)) (rest f) [(second f)])]
          (doseq [n names]
            (when (and (symbol? n) (contains? (ns-refers ns-obj) n))
              (finding! ctx :warning (line-of f nil) (str n " shadows clojure.core/" n)))
            (let [v (intern ns-obj (with-meta n {}))]
              (when (contains? '#{defn defn-} (first f))
                (alter-meta! v assoc :arglists (declared-arglists f)))
              (when (= 'defn- (first f)) (swap! private conj [n v (line-of f nil)])))))))
    (doseq [f forms]
      (try
        (analyze f {} ctx (line-of f nil))
        (catch Throwable t
          (finding! ctx :warning (line-of f nil)
                    (str "this form could not be fully analysed: " (.getMessage t))))))
    (doseq [[n v line] @private]
      (when-not (contains? @(:used-vars ctx) v)
        (finding! ctx :warning line (str "private function " n " is never used"))))
    @(:findings ctx)))

;; ---------------------------------------------------------------------------------------------
;; Results: bounded realization, structural comparison, and contract checks

(defn- realize!
  "Forces every lazy part of `x`, counting elements against `budget` (a one-element long array)."
  [x ^longs budget depth]
  (when (> depth 512)
    (throw (ex-info "the result is nested more than 512 levels deep" {:phase :realize})))
  (let [tick (fn []
               (aset budget 0 (dec (aget budget 0)))
               (when (neg? (aget budget 0))
                 (throw (ex-info "too many elements" {:phase :realize :too-large true}))))]
    (cond
      (map? x) (doseq [[k v] x] (tick) (realize! k budget (inc depth)) (realize! v budget (inc depth)))
      (or (coll? x) (seq? x)) (loop [s (seq x)]
                                (when s
                                  (tick)
                                  (realize! (first s) budget (inc depth))
                                  (recur (next s))))
      :else nil)))

(defn- describe [x]
  (cond
    (nil? x) "nil"
    (boolean? x) "a boolean"
    (string? x) "a string"
    (keyword? x) "a keyword"
    (symbol? x) "a symbol"
    (char? x) "a character"
    (or (instance? Double x) (instance? Float x)) "a floating-point number"
    (decimal? x) "a BigDecimal"
    (ratio? x) "a ratio"
    (integer? x) "an integer"
    (map? x) "a map"
    (set? x) "a set"
    (vector? x) "a vector"
    (list? x) "a list"
    (seq? x) "a sequence"
    (sequential? x) "a sequential collection"
    :else (str "an instance of " (.getName (class x)))))

(defn- num-kind [x]
  (cond
    (or (instance? Double x) (instance? Float x)) :float
    (decimal? x) :decimal
    (rational? x) :rational
    :else :other))

(defn- floats-equal? [a b tolerance]
  (let [a (double a) b (double b)]
    (or (== a b)
        (and (Double/isNaN a) (Double/isNaN b))
        (and (not (Double/isInfinite a)) (not (Double/isInfinite b))
             (<= (Math/abs (- a b))
                 (max 1e-12 (* tolerance (max (Math/abs a) (Math/abs b)))))))))

(defn- at [path]
  (if (empty? path) "the output" (str "at " (pr-str path))))

(declare difference)

(defn- seq-difference [e a path opts]
  (loop [i 0 e (seq e) a (seq a)]
    (cond
      (and (nil? e) (nil? a)) nil
      (nil? e) (str (at path) ": expected " i " elements but got more, starting with " (show (first a)))
      (nil? a) (str (at path) ": expected " (+ i (count e)) " elements but got " i
                    "; missing " (show (first e)))
      :else (or (difference (first e) (first a) (conj path i) opts)
                (recur (inc i) (next e) (next a))))))

(defn- unordered-difference [e a path opts]
  (let [e (vec e) a (vec a)]
    (cond
      (= (frequencies e) (frequencies a)) nil
      (not= (count e) (count a))
      (str (at path) ": expected " (count e) " elements (in any order) but got " (count a))
      (> (* (count e) (count a)) 4000000)
      (str (at path) ": the elements differ (compared in any order)")
      :else
      (loop [pending e remaining a]
        (if-let [x (first pending)]
          (let [i (first (keep-indexed (fn [i y] (when-not (difference x y [] opts) i)) remaining))]
            (if (nil? i)
              (str (at path) ": no element matches the expected " (show x) " (compared in any order)")
              (recur (rest pending) (into (subvec remaining 0 i) (subvec remaining (inc i))))))
          nil)))))

(defn difference
  "nil when `actual` equals `expected` under the harness's equality rules, otherwise a
   description of the first difference found."
  [expected actual path opts]
  (cond
    (and (number? expected) (number? actual))
    (let [ke (num-kind expected) ka (num-kind actual)]
      (cond
        (not= ke ka)
        (str (at path) ": expected " (describe expected) " " (pr-str expected)
             " but got " (describe actual) " " (pr-str actual))
        (= ke :float) (when-not (floats-equal? expected actual (:float-tolerance opts))
                        (str (at path) ": expected " (pr-str expected) " but got " (pr-str actual)))
        (= ke :decimal) (when-not (zero? (.compareTo ^BigDecimal expected ^BigDecimal actual))
                          (str (at path) ": expected " (pr-str expected) " but got " (pr-str actual)))
        :else (when-not (= expected actual)
                (str (at path) ": expected " (pr-str expected) " but got " (pr-str actual)))))

    (and (map? expected) (map? actual))
    (let [missing (remove #(contains? actual %) (keys expected))
          extra (remove #(contains? expected %) (keys actual))]
      (cond
        (seq missing) (str (at path) ": missing key " (show (first missing))
                           (when (next missing) (str " (and " (dec (count missing)) " more)")))
        (seq extra) (str (at path) ": unexpected key " (show (first extra))
                         (when (next extra) (str " (and " (dec (count extra)) " more)")))
        :else (some (fn [[k v]] (difference v (get actual k) (conj path k) opts)) expected)))

    (and (set? expected) (set? actual))
    (when-not (= expected actual)
      (let [missing (remove #(contains? actual %) expected)
            extra (remove #(contains? expected %) actual)]
        (str (at path) ": "
             (cond
               (seq missing) (str "missing element " (show (first missing)))
               :else (str "unexpected element " (show (first extra)))))))

    (and (sequential? expected) (sequential? actual))
    (if (and (empty? path) (:unordered opts))
      (unordered-difference expected actual path opts)
      (seq-difference expected actual path opts))

    (and (inst? expected) (inst? actual))
    (when-not (= (inst-ms expected) (inst-ms actual))
      (str (at path) ": expected " (pr-str expected) " but got " (pr-str actual)))

    (= expected actual) nil

    :else (letfn [(typed [x] (if (nil? x) "nil" (str (describe x) " " (show x))))]
            (str (at path) ": expected " (typed expected) " but got " (typed actual)))))

(def ^:private type-checks
  {:nil [nil? "nil"]
   :boolean [boolean? "a boolean"]
   :integer [integer? "an integer"]
   :number [number? "a number"]
   :string [string? "a string"]
   :keyword [keyword? "a keyword"]
   :symbol [symbol? "a symbol"]
   :map [map? "a map"]
   :vector [vector? "a vector"]
   :list [seq? "a list or sequence"]
   :set [set? "a set"]
   :sequential [sequential? "a sequential collection"]})

(defn- type-problem [t x]
  (when-let [[pred label] (get type-checks t)]
    (when-not (pred x) (str "must be " label " but is " (describe x)))))

(defn- key-problems [where m key-specs]
  (for [{:keys [key required type] nil-rule :nil} key-specs
        :let [present (contains? m key)
              v (get m key)
              problem (cond
                        (and required (not present)) (str "required key " (pr-str key) " is missing")
                        (not present) nil
                        (clojure.core/nil? v) (when (= nil-rule :disallowed)
                                                (str "key " (pr-str key) " must not be nil"))
                        :else (when-let [p (type-problem type v)]
                                (str "key " (pr-str key) " " p)))]
        :when problem]
    (str where problem)))

(defn contract-problems
  "Violations of the output shape in `contract` by `value`, at most 20."
  [contract value]
  (let [{:keys [type keys]} (:output contract)
        whole (when-let [p (type-problem type value)] [(str "the output " p)])
        key-specs (map #(update % :key edn/read-string) keys)
        per-key (when (seq key-specs)
                  (cond
                    (map? value) (key-problems "" value key-specs)
                    (coll? value) (mapcat (fn [i x]
                                            (when (map? x) (key-problems (str "element " i ": ") x key-specs)))
                                          (range) value)
                    :else nil))]
    (vec (take 20 (concat whole per-key)))))

;; ---------------------------------------------------------------------------------------------
;; Calls

(defn- frame-info
  "Where in the candidate's namespace `t` was thrown (line), and the Clojure function it surfaced
   in, from the stack trace: a clojure.core function, or for inlined ones (such as inc) the
   runtime method."
  [^Throwable t ns-sym]
  (let [prefix (str (munge (str ns-sym)) "$")
        frames (seq (.getStackTrace t))
        class-of (fn [^StackTraceElement e] (.getClassName e))
        own? (fn [e] (.startsWith ^String (class-of e) prefix))
        user (first (filter own? frames))
        ;; Every frame in the candidate's code, innermost first: the calls that were running.
        own-lines (->> frames
                       (filter own?)
                       (map #(.getLineNumber ^StackTraceElement %))
                       (filter pos?)
                       dedupe
                       (take 8)
                       vec)
        before (take-while (complement own?) frames)
        core (first (filter #(.startsWith ^String (class-of %) "clojure.core$") before))
        ;; The runtime method the candidate's code called directly, not its internal helpers.
        lang (last (filter #(.startsWith ^String (class-of %) "clojure.lang.") before))]
    (cond-> {}
      (and user (pos? (.getLineNumber ^StackTraceElement user)))
      (assoc :line (.getLineNumber ^StackTraceElement user))
      (seq own-lines)
      (assoc :lines own-lines)
      core
      (assoc :in (str "clojure.core/"
                      (clojure.lang.Compiler/demunge
                       (first (string/split (subs (class-of core) 13) #"\$")))))
      (and (not core) lang)
      (assoc :in (str (class-of lang) "." (.getMethodName ^StackTraceElement lang))))))

(def max-captured-chars 65536)
(def max-shown-chars 20000)

(defn- bounded-writer
  "A writer that keeps at most `limit` characters and fails the call once it is exceeded."
  [^StringWriter sw limit]
  (proxy [java.io.Writer] []
    (write
      ([x]
       (let [s (if (string? x) x (if (integer? x) (str (char x)) (String. ^chars x)))]
         (.write ^Writer this s 0 (count s))))
      ([x off len]
       (when (> (+ (.length (.getBuffer sw)) len) limit)
         (throw (ex-info "printed output exceeds the capture limit" {:phase :call})))
       (if (string? x)
         (.write sw ^String x (int off) (int len))
         (.write sw ^chars x (int off) (int len)))))
    (flush [])
    (close [])))

(defn- in-thread
  "Runs `f` on its own thread with a deep stack; returns its result or ::timeout after `ms`."
  [f ms]
  (let [result (promise)
        t (Thread. nil
                   (fn [] (deliver result (try {:ok (f)} (catch Throwable e {:error e}))))
                   "clojurecraft-call"
                   (* 16 1024 1024))]
    (.setDaemon t true)
    (.start t)
    (.join t (long ms))
    (if (realized? result) @result ::timeout)))

(defn- invoke
  "Calls f with the input, or with the input's elements as arguments."
  [f input spread]
  (if spread (apply f input) (f input)))

(defn- call-once [f {:keys [input expected]} {:keys [call-ms max-elements equality contract] :as opts}]
  (let [sw (StringWriter.)
        out (bounded-writer sw max-captured-chars)
        started (System/nanoTime)
        expected-value (when expected
                         (try {:value (read-value expected)}
                              (catch Throwable t {:error t})))
        run (in-thread
             (fn []
               (let [in (read-value input)
                     v (binding [*out* out] (invoke f in (:spread opts)))]
                 ;; Lazy results are realized inside the time limit, so their errors and cost
                 ;; belong to this call.
                 (realize! v (long-array [max-elements]) 0)
                 v))
             call-ms)
        elapsed (- (System/nanoTime) started)
        base {:out (str sw) :elapsed-ns elapsed}
        throws (let [e (:value expected-value)] (when (instance? Throws e) (:class-name e)))]
    (cond
      (= run ::timeout)
      (assoc base :status "timeout")

      (and throws (:error run) (throws-match? throws (:error run)))
      (assoc base :status "ok" :truncated false :match true
             :actual (str "#throws " (.getName (class (:error run)))))

      (:error run)
      (let [t (:error run)]
        (if (:too-large (ex-data t))
          (assoc base :status "too-large"
                 :message (str "the result has more than " max-elements
                               " elements; it may be an infinite lazy sequence"))
          (assoc base :status "error"
                 :error (merge (error-map :call t) (frame-info t (:ns opts))))))

      :else
      (let [v (:ok run)
            ;; With :output-chars the value is printed without length or nesting bounds (it is
            ;; already realized within the element limit) and cut only at that many characters,
            ;; so a print that is not truncated is the exact value.
            [shown truncated] (if-let [n (:output-chars opts)]
                                (let [s (binding [*print-length* nil *print-level* nil
                                                  *print-namespace-maps* false]
                                          (pr-str v))]
                                  (if (> (count s) n) [(subs s 0 n) true] [s false]))
                                [(bounded-str v 100000 1000 max-shown-chars) false])
            ;; Comparing can itself fail, for example looking up a key in a sorted map of
            ;; another key type.
            guarded (fn [f] (try (f) (catch Throwable t (str "could not compare: " (.getMessage t)))))]
        (cond-> (assoc base :status "ok" :actual shown :truncated truncated ::value v)
          (:error expected-value)
          (assoc :status "error"
                 :error {:phase "expected" :class "" :message (str "the expected value is not valid EDN: "
                                                                    (.getMessage ^Throwable (:error expected-value)))})
          throws
          (assoc :match false
                 :difference (str "expected " throws " to be thrown, but the call returned " shown))

          (and (contains? expected-value :value) (not throws))
          (merge (let [d (guarded #(difference (:value expected-value) v [] equality))]
                   {:match (nil? d) :difference d}))
          contract
          (assoc :contract (let [p (guarded #(contract-problems contract v))]
                             (if (string? p) [p] p))))))))

(defn- call-outcome
  "What a call produced, kept so that an instrumented run can be compared with it."
  [r]
  (assoc (case (:status r)
           "ok" (if (contains? r ::value) {:value (::value r)} {:other "ok"})
           "error" {:error (select-keys (:error r) [:class :message])}
           {:other (:status r)})
         ;; Whether the call failed its check: an error, a wrong value, or a contract violation.
         :failed (or (not= "ok" (:status r)) (false? (:match r)) (boolean (seq (:contract r))))))

;; ---------------------------------------------------------------------------------------------
;; Intermediate values. An instrumented copy of the code (the checked code with probe calls
;; around selected expressions) records what those expressions evaluate to while each call runs.
;; The copy is used only when removing the probes gives back exactly the checked forms, and each
;; call's values are reported together with whether its result equals the result of the
;; uninstrumented code.

(def ^:private probe-state (atom nil))
(def ^:private probe-values-kept 3)
(def ^:private probe-entries-kept 8)
(def ^:private probe-print-length 20)
(def ^:private probe-print-level 6)
(def ^:private probe-print-chars 4000)

(defn- fully-shown?
  "Whether printing `x` within the probe bounds shows all of it."
  [x depth]
  (cond
    (map? x) (and (< depth probe-print-level)
                  (<= (bounded-count (inc probe-print-length) x) probe-print-length)
                  (every? (fn [[k v]] (and (fully-shown? k (inc depth)) (fully-shown? v (inc depth)))) x))
    (or (coll? x) (seq? x)) (and (< depth probe-print-level)
                                 (<= (bounded-count (inc probe-print-length) x) probe-print-length)
                                 (every? #(fully-shown? % (inc depth)) (take probe-print-length x)))
    :else true))

(defn- preview
  "A bounded print of an intermediate value, and whether it shows the whole value."
  [x]
  (if (fn? x)
    {:kind "function" :text "" :complete false}
    (let [sw (StringWriter.)
          w (bounded-writer sw probe-print-chars)]
      (try
        (binding [*print-length* probe-print-length *print-level* probe-print-level
                  *print-namespace-maps* false *out* w]
          (pr x))
        {:kind "value" :text (str sw) :complete (fully-shown? x 0)}
        (catch clojure.lang.ExceptionInfo e
          (if (= "printed output exceeds the capture limit" (.getMessage e))
            {:kind "value" :text (str sw) :complete false}
            {:kind "error" :text (str sw) :complete false :message (str (.getMessage e))}))
        (catch Throwable t
          {:kind "error" :text (str sw) :complete false :message (str (.getMessage t))})))))

(defn probe!
  "Records `v` as a value of probe `id` during an instrumented call and returns it unchanged."
  [id v]
  ;; Printing a lazy value runs code that records other probes, so the value is printed before
  ;; the state is read and the update is a swap.
  (when @probe-state
    (let [kept? (< (count (get-in @probe-state [id :values])) probe-values-kept)
          shown (when kept? (preview v))]
      (swap! probe-state
             (fn [state]
               (when state
                 (let [{:keys [n values] :or {n 0 values []}} (get state id)]
                   (assoc state id {:n (inc n)
                                    :values (if (and shown (< (count values) probe-values-kept))
                                              (conj values shown)
                                              values)})))))))
  v)

(defn probe-first
  "probe! with the value first, for use as a step of `->`."
  [v id]
  (probe! id v))

(defn record-locals!
  "Records the current values of the locals `names` as probe `id`: the latest values, so after an
   exception they are those of the scope that was running, and those of the first few entries,
   so a wrong element of a result can be traced to the entry that built it."
  [id names values]
  (swap! probe-state
         (fn [state]
           (when state
             (let [{:keys [n entries] :or {n 0 entries []}} (get state id)
                   current (mapv vector names values)]
               (assoc state id {:n (inc n)
                                :locals current
                                :entries (if (< (count entries) probe-entries-kept)
                                           (conj entries current)
                                           entries)}))))))

(defmacro probe-locals!
  "Records the locals `names` as probe `id` whenever evaluation reaches `expr`, then evaluates
   `expr` (in tail position, so `recur` inside it still works)."
  [id names expr]
  `(do (record-locals! ~id ~(mapv str names) ~names) ~expr))

(def ^:private probe-heads '#{clojurecraft.harness/probe! clojurecraft.harness/probe-first})

(defn- threaded-probe? [x]
  (and (seq? x) (contains? probe-heads (first x)) (= 2 (count x))))

(defn- strip-probes [form]
  (walk/postwalk
   (fn [x]
     (cond
       (and (seq? x) (= 'clojurecraft.harness/probe! (first x)) (= 3 (count x))) (nth x 2)
       (and (seq? x) (= 'clojurecraft.harness/probe-locals! (first x)) (= 4 (count x))) (nth x 3)
       (and (seq? x) (some threaded-probe? x)) (apply list (remove threaded-probe? x))
       :else x))
   form))

(defn- comparable
  "Forms with reader-generated names and regular expressions made comparable between reads."
  [form]
  (walk/postwalk
   (fn [x]
     (cond
       (instance? java.util.regex.Pattern x) (list ::regex (str x))
       (and (symbol? x) (nil? (namespace x)) (re-matches #"(p\d+|rest)__\d+#" (name x)))
       (symbol (str (second (re-matches #"(p\d+|rest)__\d+#" (name x))) "__#"))
       :else x))
   form))

(defn- same-outcome? [original run]
  (cond
    (contains? original :value) (and (contains? run :ok)
                                     (nil? (difference (:value original) (:ok run) []
                                                       {:float-tolerance 0.0 :unordered false})))
    (contains? original :error) (and (contains? run :error)
                                     (= (:class (:error original))
                                        (:class (error-map :call (:error run)))))
    :else false))

(defn- run-probes [{:keys [forms probe-code fname calls outcomes opts failing-only]}]
  (let [{:keys [call-ms max-elements]} opts
        setup (try
                (let [probe-forms (read-all probe-code)]
                  (if (= (comparable (map strip-probes probe-forms)) (comparable forms))
                    (binding [*ns* (create-ns 'clojurecraft.scratch)]
                      (refer-clojure)
                      (doseq [form probe-forms] (eval form))
                      (let [v (ns-resolve (the-ns (second (first forms))) (symbol fname))]
                        (if (and v (fn? @v))
                          {:fn @v}
                          {:error (str "no public function named " fname)})))
                    {:error "the instrumented code is not the checked code with probes added"}))
                (catch Throwable t
                  {:error (str "the instrumented code could not be loaded: " (.getMessage t))}))]
    (if-let [e (:error setup)]
      (emit {:event "probe-setup" :ok false :message e})
      (do
        (emit {:event "probe-setup" :ok true})
        (doseq [[i call original] (map vector (range) calls outcomes)
                :when (or (not failing-only) (:failed original))]
          (reset! probe-state {})
          (let [run (in-thread
                     (fn []
                       (let [v (invoke (:fn setup) (read-value (:input call)) (:spread opts))]
                         (realize! v (long-array [max-elements]) 0)
                         v))
                     call-ms)
                captured @probe-state]
            (reset! probe-state nil)
            (emit {:event "probe"
                   :index i
                   :status (cond (= run ::timeout) "timeout" (:error run) "error" :else "ok")
                   :message (when-let [t (and (map? run) (:error run))] (str (.getMessage ^Throwable t)))
                   :consistent (and (not= run ::timeout) (same-outcome? original run))
                   :probes (vec (for [[id {:keys [n values locals entries]}] (sort-by key captured)]
                                  (let [shown (fn [ls] (mapv (fn [[nm v]] (assoc (preview v) :name nm)) ls))]
                                    (cond-> {:id id :count n :values (or values [])}
                                      locals (assoc :locals (shown locals))
                                      (next entries) (assoc :entries (mapv shown entries))))))})
            (when (= run ::timeout)
              (emit {:event "halted"})
              (.halt (Runtime/getRuntime) 0))))))))

(defn run-request [{code :code fname :fn calls :calls checks :checks lint? :lint :as request}]
  (let [opts (merge {:call-ms 5000 :max-elements 1000000
                     :equality {:float-tolerance 1e-9 :unordered false}}
                    (select-keys request [:call-ms :max-elements :equality :contract :output-chars :spread]))
        project (:project request)
        forms (try (read-all code)
                   (catch Throwable t (emit (assoc (error-map :read t) :event "read" :ok false)) nil))]
    (when forms
      (emit {:event "read" :ok true :forms (count forms)})
      (when-let [ns-sym (try
                          (let [n (cond
                                    (not checks) (second (first forms))
                                    project (check-project-forms forms project)
                                    :else (check-forms forms))]
                            (when checks (emit {:event "check" :ok true}))
                            n)
                          (catch Throwable t
                            (emit (assoc (error-map :check t) :event "check" :ok false))
                            nil))]
        (let [loaded (try
                       (binding [*ns* (create-ns 'clojurecraft.scratch)]
                         (refer-clojure)
                         (eval (first forms))
                         (when lint?
                           (emit {:event "lint"
                                  :findings (try (lint (the-ns ns-sym) (rest forms)
                                                       (when project (set (map symbol (:checked project)))))
                                                 (catch Throwable t
                                                   [{:level :warning :message (str "lint could not run: " (.getMessage t))}]))})))
                       (binding [*ns* (create-ns 'clojurecraft.scratch)]
                         (refer-clojure)
                         (doseq [form forms] (eval form)))

                       (let [v (ns-resolve (the-ns ns-sym) (symbol fname))]
                         (if (and v (fn? @v))
                           (do (emit {:event "load" :ok true}) @v)
                           (do (emit {:event "load" :ok false :phase "load" :class ""
                                      :message (str "no public function named " fname)})
                               nil)))
                       (catch Throwable t
                         (emit (assoc (error-map :load t) :event "load" :ok false))
                         nil))]
          (when loaded
            (let [outcomes (loop [i 0 calls (seq calls) outcomes []]
                             (if-not calls
                               outcomes
                               (do
                                 (emit {:event "start" :index i})
                                 (let [r (call-once loaded (first calls) (assoc opts :ns ns-sym))]
                                   (emit (assoc (dissoc r ::value) :event "result" :index i))
                                   (if (= "timeout" (:status r))
                                     (do (emit {:event "halted"})
                                         ;; The timed-out call cannot be stopped from outside its thread.
                                         (.halt (Runtime/getRuntime) 0))
                                     (recur (inc i) (next calls) (conj outcomes (call-outcome r))))))))]
              (when-let [probe-code (:probe-code request)]
                (run-probes {:forms forms :probe-code probe-code :fname fname :calls calls
                             :outcomes outcomes :opts opts
                             :failing-only (:probe-failing-only request)})))))))
    (emit {:event "done"})))

(let [request (edn/read-string (slurp *in*))]
  (run-request request)
  (flush))
