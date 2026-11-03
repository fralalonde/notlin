import com.sun.source.tree.*;
import com.sun.source.util.*;
import java.io.*;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import javax.lang.model.element.*;
import javax.lang.model.type.*;
import javax.tools.*;

/** Read-only attribution of the exact source strings supplied by notlin. */
public final class JavaBindings {
    private record Input(long id, String path, String hash, String source) {}
    private record Pending(Input input,TreePath path,int start,int end,String name,boolean reference) {}
    private static final class Source extends SimpleJavaFileObject {
        final Input input;
        Source(Input input) {super(Path.of(input.path()).toUri(),Kind.SOURCE);this.input=input;}
        @Override public CharSequence getCharContent(boolean ignored) {return input.source();}
        @Override public InputStream openInputStream() {return new ByteArrayInputStream(input.source().getBytes(StandardCharsets.UTF_8));}
        @Override public Reader openReader(boolean ignored) {return new StringReader(input.source());}
    }

    public static void main(String[] args) throws Exception {
        Map<String,Object> request=object(new Json(Files.readString(Path.of(args[0]))).read());
        if(!Long.valueOf(1).equals(request.get("version"))) throw new IllegalArgumentException("unsupported version");
        List<Input> inputs=new ArrayList<>();
        for(Object item:array(request.get("sources"))) {
            var value=object(item);
            inputs.add(new Input((Long)value.get("id"),string(value.get("path")),string(value.get("hash")),string(value.get("source"))));
        }
        Set<String> names=new HashSet<>(strings(request.get("names")));
        var diagnostics=new DiagnosticCollector<JavaFileObject>();
        List<Object> results=new ArrayList<>();
        List<String> foundRoots=new ArrayList<>();
        var compiler=ToolProvider.getSystemJavaCompiler();
        if(compiler==null) throw new IllegalStateException("a JDK Java compiler is required");
        try(var files=compiler.getStandardFileManager(diagnostics,Locale.ROOT,StandardCharsets.UTF_8)) {
            var options=new ArrayList<>(List.of("-classpath",String.join(File.pathSeparator,strings(request.get("classpath"))),
                "-sourcepath","","-implicit:none","-encoding","UTF-8","-Xlint:none"));
            var processors=strings(request.get("processors"));
            if(processors.isEmpty()) options.add("-proc:none");
            else options.addAll(List.of("-proc:full","-processor",String.join(",",processors)));
            var processorPath=strings(request.get("processor_path"));
            if(!processorPath.isEmpty()) options.addAll(List.of("-processorpath",String.join(File.pathSeparator,processorPath)));
            options.addAll(strings(request.get("compiler_options")));
            var sources=inputs.stream().map(Source::new).toList();
            Map<URI,Input> byUri=new HashMap<>();
            for(var source:sources) byUri.put(source.toUri(),source.input);
            var task=(JavacTask)compiler.getTask(new PrintWriter(System.err),files,diagnostics,options,null,sources);
            List<CompilationUnitTree> units=new ArrayList<>();
            task.parse().forEach(units::add);
            List<Pending> pending=new ArrayList<>();
            if(diagnostics.getDiagnostics().stream().noneMatch(d->d.getKind()==Diagnostic.Kind.ERROR)) {
                var trees=Trees.instance(task);
                var positions=trees.getSourcePositions();
                for(var unit:units) {
                    var input=Objects.requireNonNull(byUri.get(unit.getSourceFile().toUri()));
                    new TreePathScanner<Void,Void>() {
                        void emit(Tree site,Tree selected,String name,boolean reference) {
                            if(!names.contains(name)) return;
                            long end=positions.getEndPosition(unit,selected);
                            long from=positions.getStartPosition(unit,selected);
                            if(from<0 || end<from || end>input.source().length()) throw new IllegalStateException("missing original call range");
                            int start=selectorStart(input.source(),(int)from,(int)end,name);
                            if(start<0) throw new IllegalStateException("original selector range mismatch");
                            var path=selected==site?getCurrentPath():new TreePath(getCurrentPath(),selected);
                            pending.add(new Pending(input,path,start,(int)end,name,reference));
                        }
                        @Override public Void visitMethodInvocation(MethodInvocationTree call,Void unused) {
                            var selected=call.getMethodSelect();
                            if(selected instanceof MemberSelectTree member) emit(call,selected,member.getIdentifier().toString(),false);
                            else if(selected instanceof IdentifierTree identifier) emit(call,selected,identifier.getName().toString(),false);
                            return super.visitMethodInvocation(call,unused);
                        }
                        @Override public Void visitMemberReference(MemberReferenceTree reference,Void unused) {
                            emit(reference,reference,reference.getName().toString(),true);
                            return super.visitMemberReference(reference,unused);
                        }
                    }.scan(unit,null);
                }
            }
            task.analyze();
            if(diagnostics.getDiagnostics().stream().noneMatch(d->d.getKind()==Diagnostic.Kind.ERROR)) {
                var trees=Trees.instance(task);
                Map<String,TypeMirror> roots=new LinkedHashMap<>();
                for(String name:strings(request.getOrDefault("roots",List.of()))) {
                    var root=task.getElements().getTypeElement(name);
                    if(root!=null) {roots.put(name,task.getTypes().erasure(root.asType()));foundRoots.add(name);}
                }
                Map<Long,List<Object>> calls=new LinkedHashMap<>();
                for(var input:inputs) calls.put(input.id(),new ArrayList<>());
                for(var call:pending) {
                    var element=trees.getElement(call.path());
                    if(!(element instanceof ExecutableElement method)) throw new IllegalStateException("unattributed source call");
                    var owner=(TypeElement)method.getEnclosingElement();
                    var descriptor=new StringBuilder("(");
                    for(var parameter:method.getParameters()) descriptor.append(descriptor(task,parameter.asType()));
                    descriptor.append(')').append(descriptor(task,method.getReturnType()));
                    var result=new LinkedHashMap<String,Object>();
                    result.put("start_byte",call.input().source().substring(0,call.start()).getBytes(StandardCharsets.UTF_8).length);
                    result.put("end_byte",call.input().source().substring(0,call.end()).getBytes(StandardCharsets.UTF_8).length);
                    result.put("name",call.name());
                    result.put("owner_binary",task.getElements().getBinaryName(owner).toString());
                    result.put("descriptor",descriptor.toString());
                    result.put("parameter_count",method.getParameters().size());
                    result.put("is_static",method.getModifiers().contains(Modifier.STATIC));
                    result.put("reference",call.reference());
                    List<String> related=new ArrayList<>();
                    var ownerType=task.getTypes().erasure(owner.asType());
                    for(var root:roots.entrySet()) if(task.getTypes().isSubtype(ownerType,root.getValue()) || task.getTypes().isSubtype(root.getValue(),ownerType)) related.add(root.getKey());
                    result.put("related_roots",related);
                    calls.get(call.input().id()).add(result);
                }
                for(var input:inputs) results.add(Map.of("id",input.id(),"hash",input.hash(),"calls",calls.get(input.id())));
            }
        }
        List<Object> errors=new ArrayList<>();
        for(var diagnostic:diagnostics.getDiagnostics()) if(diagnostic.getKind()==Diagnostic.Kind.ERROR) {
            errors.add(Map.of("message",diagnostic.getMessage(Locale.ROOT),"line",diagnostic.getLineNumber(),"code",diagnostic.getCode()));
        }
        var response=Map.of("version",1,"token",request.get("token"),"success",errors.isEmpty(),"files",results,"errors",errors,"roots",foundRoots);
        Files.writeString(Path.of(args[1]),encode(response),StandardCharsets.UTF_8);
    }

    private static String descriptor(JavacTask task,TypeMirror type) {
        type=task.getTypes().erasure(type);
        return switch(type.getKind()) {
            case BOOLEAN->"Z";case BYTE->"B";case SHORT->"S";case INT->"I";case LONG->"J";
            case CHAR->"C";case FLOAT->"F";case DOUBLE->"D";case VOID->"V";
            case ARRAY->"["+descriptor(task,((ArrayType)type).getComponentType());
            case DECLARED->"L"+task.getElements().getBinaryName((TypeElement)((DeclaredType)type).asElement()).toString().replace('.','/')+";";
            default->throw new IllegalStateException("unresolved JVM type: "+type);
        };
    }

    // Javac positions count UTF-16 code units in the original source. Decode
    // Unicode escapes while keeping their original positions, including an
    // escaped method name after a receiver that contains non-ASCII text.
    private static int selectorStart(String source,int from,int end,String name) {
        var decoded=new StringBuilder();var positions=new ArrayList<Integer>();
        for(int at=from;at<end;) {
            int start=at;char value=source.charAt(at++);
            if(value=='\\' && at<end && source.charAt(at)=='u') {
                while(at<end && source.charAt(at)=='u') at++;
                if(at+4<=end) {
                    try {value=(char)Integer.parseInt(source.substring(at,at+4),16);at+=4;}
                    catch(NumberFormatException ignored) {at=start+1;}
                }
            }
            decoded.append(value);positions.add(start);
        }
        int offset=decoded.length()-name.length();
        return offset>=0 && decoded.substring(offset).equals(name)?positions.get(offset):-1;
    }

    @SuppressWarnings("unchecked") private static Map<String,Object> object(Object value) {return (Map<String,Object>)value;}
    @SuppressWarnings("unchecked") private static List<Object> array(Object value) {return (List<Object>)value;}
    private static String string(Object value) {return (String)value;}
    private static List<String> strings(Object value) {return array(value).stream().map(JavaBindings::string).toList();}
    private static String encode(Object value) {
        if(value==null)return "null";
        if(value instanceof String text) {
            var out=new StringBuilder("\"");
            for(char c:text.toCharArray()) switch(c) {
                case '"'->out.append("\\\"");case '\\'->out.append("\\\\");
                case '\n'->out.append("\\n");case '\r'->out.append("\\r");case '\t'->out.append("\\t");
                default->{if(c<32)out.append(String.format(Locale.ROOT,"\\u%04x",(int)c));else out.append(c);}
            }
            return out.append('"').toString();
        }
        if(value instanceof Map<?,?> map) return "{"+String.join(",",map.entrySet().stream().map(e->encode(e.getKey())+":"+encode(e.getValue())).toList())+"}";
        if(value instanceof Iterable<?> list) {var values=new ArrayList<String>();for(Object item:list)values.add(encode(item));return "["+String.join(",",values)+"]";}
        if(value instanceof Boolean || value instanceof Number)return value.toString();
        throw new IllegalArgumentException("unsupported JSON value");
    }

    private static final class Json {
        final String text;int at;
        Json(String text){this.text=text;}
        Object read(){Object value=value();space();if(at!=text.length())throw new IllegalArgumentException("trailing JSON");return value;}
        void space(){while(at<text.length() && Character.isWhitespace(text.charAt(at)))at++;}
        char take(){space();if(at==text.length())throw new IllegalArgumentException("incomplete JSON");return text.charAt(at++);}
        void expect(char value){if(take()!=value)throw new IllegalArgumentException("unexpected JSON token");}
        Object value() {
            space();char c=text.charAt(at);
            if(c=='"')return quoted();
            if(c=='{') {at++;var result=new LinkedHashMap<String,Object>();space();if(text.charAt(at)=='}'){at++;return result;}
                do {String key=quoted();expect(':');if(result.containsKey(key))throw new IllegalArgumentException("duplicate key");result.put(key,value());char next=take();if(next=='}')return result;if(next!=',')throw new IllegalArgumentException("object separator");}while(true);}
            if(c=='[') {at++;var result=new ArrayList<Object>();space();if(text.charAt(at)==']'){at++;return result;}
                do {result.add(value());char next=take();if(next==']')return result;if(next!=',')throw new IllegalArgumentException("array separator");}while(true);}
            for(String literal:List.of("true","false","null"))if(text.startsWith(literal,at)){at+=literal.length();return literal.equals("null")?null:literal.equals("true");}
            int start=at;if(c=='-')at++;while(at<text.length() && Character.isDigit(text.charAt(at)))at++;
            if(start==at)throw new IllegalArgumentException("JSON value");return Long.parseLong(text.substring(start,at));
        }
        String quoted() {
            expect('"');var out=new StringBuilder();
            while(at<text.length()) {char c=text.charAt(at++);if(c=='"')return out.toString();
                if(c=='\\') {c=text.charAt(at++);switch(c) {
                    case '"','\\','/'->out.append(c);case 'b'->out.append('\b');case 'f'->out.append('\f');
                    case 'n'->out.append('\n');case 'r'->out.append('\r');case 't'->out.append('\t');
                    case 'u'->{out.append((char)Integer.parseInt(text.substring(at,at+4),16));at+=4;}
                    default->throw new IllegalArgumentException("JSON escape");
                }} else {if(c<32)throw new IllegalArgumentException("JSON control character");out.append(c);}
            }
            throw new IllegalArgumentException("unterminated JSON string");
        }
    }
}
